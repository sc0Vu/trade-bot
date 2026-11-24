//! Simulated execution for margined instruments: linear perpetuals
//! (Binance USDⓈ-M, one-way mode) and CFDs (eToro): [`PerpPaperBroker`].
//!
//! **CFDs** use the same broker with [`PerpTerms::cfd`]: isolated margin
//! per position, no funding, and instead of bracket liquidation a
//! close-out where the loss reaches half the position's margin (eToro's
//! terms, [`bot_core::margin::close_out_price`]), checked on the trade
//! bar. It fills like a stop at market: at the close-out price, or at the
//! bar's open when the price gapped past it; ordinary taker fee, no
//! clearance; the whole loss is the trader's (a gap can take more than
//! the margin), the wallet flooring at zero (negative balance protection).
//!
//! Orders fill exactly as in [`super::PaperBroker`] (same [`FillModel`],
//! [`SlippageModel`] and [`FeeModel`]; fees are paid in the quote), but
//! nothing is bought outright: a fill opens, adds to, reduces or flips a
//! signed [`Position`] (`Position::apply` for perpetuals), and margin moves
//! instead of cash:
//!
//! - **Isolated** (per symbol): opening `Δ` units moves `Δ × price /
//!   leverage` from the wallet into the position's own margin; closing a
//!   share of the position returns that share of its margin. Realized P&L
//!   and fees settle to the wallet. Funding is paid from (or to) the
//!   position's margin. At most that margin can be lost.
//! - **Cross**: positions share the wallet. Available = wallet + unrealized
//!   P&L of cross positions − their initial margin.
//!
//! An order is refused (`Rejected`) when the leverage is above what the
//! symbol's bracket allows at the resulting size, or the margin to open it
//! (plus the taker fee) isn't available. Selling from flat opens a short.
//!
//! **Liquidation.** After each candle's fills, every position on the
//! instrument is checked against its liquidation price
//! ([`bot_core::margin::liquidation_price`]) at the bar's worst price for
//! it (a long's low, a short's high): the mark-price bar of the same open
//! time (`MarketEvent::Mark`, as the venue liquidates on mark) when one
//! was delivered, else the trade bar (the cautious stand-in). A breached
//! position is closed at its liquidation price (rounded to tick against
//! the trader) with a clearance fee of `clearance` × notional (an
//! assumption the caller states: Binance's varies by symbol), taken from
//! the margin left at that price and capped by it (isolated: the
//! position's margin; cross: the wallet), so nothing is lost past it.
//! Should a loss still exceed it, isolated loses its margin only and the
//! cross wallet floors at zero (the venue's insurance fund). The fill
//! carries order id
//! `liq-N`.

use super::broker::cap_at_limit;
use super::{Decision, FeeModel, FillModel, OrderState, SlippageModel};
use crate::{Broker, ExchangeError};
use async_trait::async_trait;
use bot_core::margin::{self, Brackets, MarginMode};
use bot_core::{
    AssetClass, Candle, Decimal, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity,
    MarketEvent, Notional, Order, OrderId, Position, Price, Qty, Side, Size,
};
use std::collections::HashMap;

/// A symbol's margin settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerpTerms {
    /// Leverage used to size margin (≥ 1).
    pub leverage: u32,
    pub mode: MarginMode,
    pub brackets: Brackets,
    /// `None`: liquidation by the brackets (Binance). `Some(ratio)`: a CFD's
    /// close-out (eToro): the position closes where its loss reaches
    /// `ratio` of its margin ([`margin::close_out_price`]); see the module
    /// docs.
    pub close_out: Option<Decimal>,
}

impl PerpTerms {
    /// Terms for an eToro CFD at `leverage`: isolated, closed out at
    /// [`margin::CFD_CLOSE_OUT`] of the margin, no maintenance brackets
    /// (one tier, rate 0, allowing up to `leverage`: the allowed leverages
    /// are the venue's eligibility list, checked by the caller).
    pub fn cfd(leverage: u32) -> Result<Self, ExchangeError> {
        let brackets = Brackets::new(vec![margin::Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(Decimal::from(1_000_000_000_000_i64)),
            max_leverage: leverage.max(1),
            rate: Decimal::ZERO,
            cum: Decimal::ZERO,
        }])
        .map_err(|_| ExchangeError::Rejected("bad CFD terms"))?;
        Ok(Self {
            leverage,
            mode: MarginMode::Isolated,
            brackets,
            close_out: Some(margin::CFD_CLOSE_OUT),
        })
    }
}

struct Pending {
    order: Order,
    state: OrderState,
    /// Margin plus fee held for it until it fills or is cancelled.
    reserved: Notional,
}

/// See the module docs.
pub struct PerpPaperBroker {
    instruments: HashMap<InstrumentId, Instrument>,
    terms: HashMap<InstrumentId, PerpTerms>,
    fill_model: Box<dyn FillModel>,
    slippage: Box<dyn SlippageModel>,
    fees: Box<dyn FeeModel>,
    clearance: Decimal,
    wallet: Notional,
    isolated: HashMap<InstrumentId, Notional>,
    positions: HashMap<InstrumentId, Position>,
    pending: Vec<Pending>,
    marks: HashMap<InstrumentId, Price>,
    /// The latest mark-price bar per instrument (`MarketEvent::Mark`).
    mark_bars: HashMap<InstrumentId, Candle>,
    next_liq: u64,
}

impl PerpPaperBroker {
    /// A broker for `instruments` (all perpetuals, each with `terms`),
    /// starting with `wallet` in the quote. `clearance` is the liquidation
    /// clearance fee rate assumed (`0.0125`).
    pub fn new(
        instruments: Vec<Instrument>,
        terms: HashMap<InstrumentId, PerpTerms>,
        fill_model: Box<dyn FillModel>,
        slippage: Box<dyn SlippageModel>,
        fees: Box<dyn FeeModel>,
        wallet: Notional,
        clearance: Decimal,
    ) -> Result<Self, ExchangeError> {
        for i in &instruments {
            if !i.is_margined() {
                return Err(ExchangeError::Rejected("not a perpetual or CFD"));
            }
            let t = terms
                .get(&i.id)
                .ok_or(ExchangeError::Rejected("perpetual without margin terms"))?;
            // A CFD is closed out per position (eToro), never by brackets;
            // its margin is its own.
            if i.class == AssetClass::Cfd
                && (t.close_out.is_none() || t.mode != MarginMode::Isolated)
            {
                return Err(ExchangeError::Rejected(
                    "a CFD needs isolated terms with a close-out",
                ));
            }
            if t.leverage == 0 || t.leverage > t.brackets.max_leverage(Notional::ZERO) {
                return Err(ExchangeError::Rejected("leverage outside the brackets"));
            }
        }
        if clearance < Decimal::ZERO || clearance >= Decimal::ONE || wallet.is_negative() {
            return Err(ExchangeError::Rejected("bad wallet or clearance fee"));
        }
        Ok(Self {
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            terms,
            fill_model,
            slippage,
            fees,
            clearance,
            wallet,
            isolated: HashMap::new(),
            positions: HashMap::new(),
            pending: Vec::new(),
            marks: HashMap::new(),
            mark_bars: HashMap::new(),
            next_liq: 1,
        })
    }

    /// Quote not held as isolated margin, realized P&L and fees included.
    pub fn wallet(&self) -> Notional {
        self.wallet
    }

    /// Margin held by an isolated position.
    pub fn isolated_margin(&self, id: &InstrumentId) -> Notional {
        self.isolated.get(id).copied().unwrap_or(Notional::ZERO)
    }

    fn pos(&self, id: &InstrumentId) -> Position {
        self.positions
            .get(id)
            .cloned()
            .unwrap_or_else(|| Position::flat(id.clone()))
    }

    fn upnl(&self, id: &InstrumentId) -> Notional {
        match self.marks.get(id) {
            Some(m) => margin::unrealized_pnl(&self.pos(id), *m),
            None => Notional::ZERO,
        }
    }

    fn mode(&self, id: &InstrumentId) -> MarginMode {
        self.terms.get(id).map_or(MarginMode::Isolated, |t| t.mode)
    }

    /// Initial margin and unrealized P&L of cross positions, `except` one.
    fn cross_others(&self, except: Option<&InstrumentId>) -> (Notional, Notional, Notional) {
        let mut im = Notional::ZERO;
        let mut mm = Notional::ZERO;
        let mut upnl = Notional::ZERO;
        for (id, p) in &self.positions {
            if Some(id) == except || p.is_flat() || self.mode(id) != MarginMode::Cross {
                continue;
            }
            let t = &self.terms[id];
            im += margin::initial_margin(p.cost_basis(), t.leverage);
            let mark = self.marks.get(id).copied().unwrap_or_default();
            mm += t
                .brackets
                .maintenance(Notional::new(p.qty().get() * mark.get()));
            upnl += self.upnl(id);
        }
        (im, mm, upnl)
    }

    /// What a new order may still use: cross = wallet + cross unrealized
    /// P&L − cross initial margin; isolated = the wallet; less what
    /// pending orders hold.
    fn available(&self, id: &InstrumentId) -> Notional {
        let held: Notional = self.pending.iter().map(|p| p.reserved).sum();
        let base = match self.mode(id) {
            MarginMode::Cross => {
                let (im, _, upnl) = self.cross_others(None);
                self.wallet + upnl - im
            }
            MarginMode::Isolated => self.wallet,
        };
        base - held
    }

    /// Where `id` is liquidated now; `None` flat or never.
    pub fn liquidation_price(&self, id: &InstrumentId) -> Option<Price> {
        let p = self.pos(id);
        let t = self.terms.get(id)?;
        if let Some(ratio) = t.close_out {
            return margin::close_out_price(&p, t.leverage, ratio);
        }
        match t.mode {
            MarginMode::Isolated => margin::liquidation_price(
                &p,
                self.isolated_margin(id),
                Notional::ZERO,
                Notional::ZERO,
                &t.brackets,
            ),
            MarginMode::Cross => {
                let (_, mm, upnl) = self.cross_others(Some(id));
                margin::liquidation_price(&p, self.wallet, mm, upnl, &t.brackets)
            }
        }
    }

    /// Whether `side` `qty` opens anything: grows the position, or
    /// crosses zero into the other side (even a smaller one). Only such an
    /// order is checked for margin; one that only reduces is never refused.
    fn opens(&self, id: &InstrumentId, side: Side, qty: Decimal) -> bool {
        let held = self.pos(id).qty().get();
        let after = match side {
            Side::Buy => held + qty,
            Side::Sell => held - qty,
        };
        after.abs() > held.abs()
            || (!after.is_zero()
                && !held.is_zero()
                && after.is_sign_negative() != held.is_sign_negative())
    }

    fn slipped(&self, inst: &Instrument, side: Side, price: Price) -> Price {
        self.slippage.apply(
            inst,
            side,
            super::Execution {
                price,
                liquidity: Liquidity::Taker,
                at_open: true,
            },
        )
    }

    fn quote_fee(
        &self,
        inst: &Instrument,
        side: Side,
        qty: Qty,
        price: Price,
        l: Liquidity,
    ) -> Notional {
        match self.fees.fee(inst, side, qty, price, l) {
            Fee::Quote(f) | Fee::Other { value: Some(f), .. } => f,
            // A perpetual's fee is in the quote: value a base fee there.
            Fee::Base(b) => inst.notional(b, price),
            Fee::Other { value: None, .. } => Notional::ZERO,
        }
    }

    /// Books a fill: position, margin, wallet. `liquidation`: an isolated
    /// position's loss stops at its margin (the rest is the venue's).
    fn book(
        &mut self,
        fill: &Fill,
        inst: &Instrument,
        liquidation: bool,
    ) -> Result<(), ExchangeError> {
        let id = fill.instrument.clone();
        let (after, iso, wallet_delta) = self.flow(fill, inst, liquidation)?;
        self.wallet += wallet_delta;
        if self.mode(&id) == MarginMode::Isolated {
            self.isolated.insert(id.clone(), iso);
        }
        self.positions.insert(id, after);
        Ok(())
    }

    /// What `fill` would do: the position after it, the isolated margin
    /// after it, and the change to the wallet. Isolated: the closed share
    /// of the margin (all of it on a flip or close) plus realized P&L and
    /// fee come back, margin for what opened (units × fill price /
    /// leverage) goes out; `liquidation`: what comes back is never below
    /// zero. Cross: realized P&L and fee.
    fn flow(
        &self,
        fill: &Fill,
        inst: &Instrument,
        liquidation: bool,
    ) -> Result<(Position, Notional, Notional), ExchangeError> {
        let id = &fill.instrument;
        let before = self.pos(id);
        let mut after = before.clone();
        after
            .apply(fill, inst)
            .map_err(|_| ExchangeError::Rejected("fill does not fit the position"))?;
        let realized = after.realized_pnl() - before.realized_pnl();
        if self.mode(id) != MarginMode::Isolated {
            return Ok((after, Notional::ZERO, realized));
        }
        let (old, new) = (before.qty().get().abs(), after.qty().get().abs());
        let flipped = !before.is_flat()
            && !after.is_flat()
            && before.qty().is_negative() != after.qty().is_negative();
        let iso = self.isolated_margin(id);
        let closed = if flipped {
            old
        } else {
            (old - new).max(Decimal::ZERO)
        };
        let release = if old.is_zero() {
            Notional::ZERO
        } else {
            iso * (closed / old)
        };
        let mut back = release + realized;
        if liquidation && back.is_negative() {
            back = Notional::ZERO;
        }
        let opened = if flipped {
            new
        } else {
            (new - old).max(Decimal::ZERO)
        };
        let lev = Decimal::from(self.terms[id].leverage);
        let alloc = Notional::new(opened * fill.price.get() / lev);
        let iso = if after.is_flat() {
            Notional::ZERO
        } else {
            iso - release + alloc
        };
        Ok((after, iso, back - alloc))
    }

    /// Free quote an order of `qty` at `price` (fee `fee`) would take if
    /// it filled: isolated, exactly what [`flow`](Self::flow) would take
    /// from the wallet (a flip gets the old margin and its P&L back first);
    /// cross, the change in initial margin less realized P&L and fee. Zero
    /// or less: it frees margin.
    fn need(
        &self,
        inst: &Instrument,
        side: Side,
        qty: Qty,
        price: Price,
        fee: Notional,
    ) -> Result<Notional, ExchangeError> {
        let probe = Fill::new(FillSpec {
            order_id: OrderId::new("margin-check")
                .map_err(|_| ExchangeError::Rejected("margin check"))?,
            instrument: inst.id.clone(),
            side,
            qty,
            price,
            fee: Fee::Quote(fee),
            liquidity: Liquidity::Taker,
            ts: bot_core::Timestamp::from_millis(0),
        })
        .map_err(|_| ExchangeError::Rejected("invalid order size or price"))?;
        let (after, _, wallet_delta) = self.flow(&probe, inst, false)?;
        if self.mode(&inst.id) == MarginMode::Isolated {
            return Ok(-wallet_delta);
        }
        let lev = self.terms[&inst.id].leverage;
        let im = |p: &Position| margin::initial_margin(p.cost_basis(), lev);
        Ok(im(&after) - im(&self.pos(&inst.id)) - wallet_delta)
    }

    /// Liquidates a position breached at `c`; the liquidation fill.
    fn liquidate(&mut self, c: &Candle) -> Option<Fill> {
        let id = &c.instrument;
        let p = self.pos(id);
        if p.is_flat() {
            return None;
        }
        let lp = self.liquidation_price(id)?;
        let long = p.qty().is_positive();
        // The venue liquidates on the mark price: its bar when there is
        // one for this bar, else the trade bar (cautious stand-in).
        let range = self
            .mark_bars
            .get(id)
            .filter(|m| m.open_time == c.open_time)
            .unwrap_or(c);
        let breached = if long {
            range.low <= lp
        } else {
            range.high >= lp
        };
        if !breached {
            return None;
        }
        let inst = self.instruments.get(id)?.clone();
        let close_out = self.terms.get(id).is_some_and(|t| t.close_out.is_some());
        // A CFD's close-out is a stop executed at market: a bar that opens
        // past it fills at that open (worse), not at the trigger.
        let lp = match (close_out, long) {
            (true, true) if range.open < lp => range.open,
            (true, false) if range.open > lp => range.open,
            _ => lp,
        };
        // Closing a long sells: price rounded down; a short buys: up.
        let (side, price) = if long {
            (Side::Sell, inst.round_price(lp, Side::Buy))
        } else {
            (Side::Buy, inst.round_price(lp, Side::Sell))
        };
        let qty = Qty::new(p.qty().get().abs());
        if close_out {
            return self.close_out(c, &inst, side, qty, price);
        }
        // The clearance fee comes out of the margin left at that price
        // (Binance), never more: isolated = its margin, cross = the wallet.
        let entry = p.cost_basis().get() / p.qty().get();
        let pnl = p.qty().get() * (price.get() - entry);
        let left = match self.mode(id) {
            MarginMode::Isolated => self.isolated_margin(id).get(),
            MarginMode::Cross => self.wallet.get(),
        } + pnl;
        let fee = Notional::new(
            (inst.notional(qty, price).get() * self.clearance)
                .min(left)
                .max(Decimal::ZERO),
        );
        let order_id = OrderId::new(format!("liq-{}", self.next_liq)).ok()?;
        self.next_liq += 1;
        let fill = Fill::new(FillSpec {
            order_id,
            instrument: id.clone(),
            side,
            qty,
            price,
            fee: Fee::Quote(fee),
            liquidity: Liquidity::Taker,
            ts: c.close_time,
        })
        .ok()?;
        tracing::warn!(instrument = %id, "paper perpetual liquidated");
        self.book(&fill, &inst, true).ok()?;
        // Cross: a loss past the wallet is the venue's insurance fund's.
        if self.wallet.is_negative() {
            self.wallet = Notional::ZERO;
        }
        Some(fill)
    }
}

impl PerpPaperBroker {
    /// A CFD's forced close at `price`: the ordinary taker fee (eToro
    /// charges its usual spread, no clearance fee), and the loss is the
    /// trader's in full, past the position's margin on a gap (the terms:
    /// "you may lose more than your deposit in a trade"); the wallet
    /// floors at zero (retail negative balance protection).
    fn close_out(
        &mut self,
        c: &Candle,
        inst: &Instrument,
        side: Side,
        qty: Qty,
        price: Price,
    ) -> Option<Fill> {
        let fee = self.quote_fee(inst, side, qty, price, Liquidity::Taker);
        let order_id = OrderId::new(format!("liq-{}", self.next_liq)).ok()?;
        self.next_liq += 1;
        let fill = Fill::new(FillSpec {
            order_id,
            instrument: inst.id.clone(),
            side,
            qty,
            price,
            fee: Fee::Quote(fee),
            liquidity: Liquidity::Taker,
            ts: c.close_time,
        })
        .ok()?;
        tracing::warn!(instrument = %inst.id, "paper CFD closed out: loss reached its margin limit");
        self.book(&fill, inst, false).ok()?;
        if self.wallet.is_negative() {
            self.wallet = Notional::ZERO;
        }
        Some(fill)
    }
}

#[async_trait]
impl Broker for PerpPaperBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        let inst = self
            .instruments
            .get(&order.instrument)
            .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?
            .clone();
        if self.pending.iter().any(|p| p.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        crate::check_reduce_only(self.pos(&order.instrument).qty().get(), &order)?;
        let reference = self
            .marks
            .get(&order.instrument)
            .copied()
            .ok_or(ExchangeError::Rejected("no price yet to check the margin"))?;
        let price = self.slipped(&inst, order.side, reference);
        let qty = match order.size {
            Size::Base(q) => q.get(),
            Size::Quote(n) => n.get() / price.get(),
        };
        let t = &self.terms[&order.instrument];
        let held = self.pos(&order.instrument).qty().get();
        let signed = match order.side {
            Side::Buy => qty,
            Side::Sell => -qty,
        };
        let after = Notional::new((held + signed).abs() * price.get());
        if t.leverage > t.brackets.max_leverage(after) {
            return Err(ExchangeError::Rejected(
                "leverage above the bracket's maximum for this size",
            ));
        }
        let opens = self.opens(&order.instrument, order.side, qty);
        let fee = self.quote_fee(&inst, order.side, Qty::new(qty), price, Liquidity::Taker);
        let reserved = self
            .need(&inst, order.side, Qty::new(qty), price, fee)?
            .max(Notional::ZERO);
        if opens && reserved > self.available(&order.instrument) {
            return Err(ExchangeError::Rejected("insufficient margin"));
        }
        let id = order.id.clone();
        self.pending.push(Pending {
            order,
            state: OrderState {
                triggered: false,
                first_look: true,
            },
            reserved,
        });
        Ok(id)
    }

    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let before = self.pending.len();
        self.pending.retain(|p| p.order.id != id);
        if self.pending.len() == before {
            return Err(ExchangeError::UnknownOrder(id));
        }
        Ok(())
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        match event {
            MarketEvent::Funding {
                instrument,
                rate,
                mark,
                ..
            } => {
                if let Some(p) = self.positions.get_mut(instrument) {
                    let paid = p.apply_funding(*rate, *mark);
                    if self.mode(instrument) == MarginMode::Isolated {
                        let iso = self.isolated_margin(instrument) + paid;
                        self.isolated.insert(instrument.clone(), iso);
                    } else {
                        self.wallet += paid;
                    }
                }
                self.marks.insert(instrument.clone(), *mark);
                Ok(Vec::new())
            }
            MarketEvent::Mark(m) => {
                self.mark_bars.insert(m.instrument.clone(), m.clone());
                Ok(Vec::new())
            }
            MarketEvent::Candle(c) => {
                let mut fills = Vec::new();
                let mut i = 0;
                while i < self.pending.len() {
                    let p = &self.pending[i];
                    if p.order.instrument != c.instrument || c.open_time <= p.order.created_at {
                        i += 1;
                        continue;
                    }
                    match self.fill_model.evaluate(&p.order, p.state, c) {
                        Decision::Fill(exec) => {
                            let p = self.pending.remove(i);
                            let inst = self.instruments[&p.order.instrument].clone();
                            let price = cap_at_limit(
                                &p.order,
                                self.slippage.apply(&inst, p.order.side, exec),
                            );
                            let qty = match p.order.size {
                                Size::Base(q) => q,
                                Size::Quote(n) => inst.round_qty(Qty::new(n.get() / price.get())),
                            };
                            // Reduce-only never opens or flips: clipped to
                            // what is left against it (the venue's rule).
                            let qty = if p.order.reduce_only() {
                                let held = self.pos(&p.order.instrument).qty().get();
                                let room = match p.order.side {
                                    Side::Buy => -held,
                                    Side::Sell => held,
                                };
                                Qty::new(qty.get().min(room.max(Decimal::ZERO)))
                            } else {
                                qty
                            };
                            if !qty.is_positive() {
                                continue;
                            }
                            let fee =
                                self.quote_fee(&inst, p.order.side, qty, price, exec.liquidity);
                            // Margin at the real fill price, against what is
                            // free now (its own reservation released): short
                            // → dropped unfilled, as the venue would refuse.
                            let opens = self.opens(&p.order.instrument, p.order.side, qty.get());
                            let need = self.need(&inst, p.order.side, qty, price, fee)?;
                            if opens && need > self.available(&p.order.instrument) {
                                tracing::warn!(order = %p.order.id, "paper perpetual fill dropped: insufficient margin");
                                continue;
                            }
                            let fill = Fill::new(FillSpec {
                                order_id: p.order.id.clone(),
                                instrument: p.order.instrument.clone(),
                                side: p.order.side,
                                qty,
                                price,
                                fee: Fee::Quote(fee),
                                liquidity: exec.liquidity,
                                ts: if exec.at_open {
                                    c.open_time
                                } else {
                                    c.close_time
                                },
                            })
                            .map_err(|_| ExchangeError::Rejected("invalid paper fill"))?;
                            if let Err(e) = self.book(&fill, &inst, false) {
                                tracing::warn!(order = %p.order.id, error = %e, "paper perpetual fill dropped");
                                continue;
                            }
                            fills.push(fill);
                        }
                        Decision::Trigger => {
                            self.pending[i].state = OrderState {
                                triggered: true,
                                first_look: true,
                            };
                            i += 1;
                        }
                        Decision::Rest => {
                            self.pending[i].state.first_look = false;
                            i += 1;
                        }
                        Decision::Expire => {
                            self.pending.remove(i);
                        }
                    }
                }
                fills.extend(self.liquidate(c));
                self.marks.insert(c.instrument.clone(), c.close);
                Ok(fills)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn position(&self, instrument: &InstrumentId) -> Position {
        self.pos(instrument)
    }

    fn open_orders(&self) -> Vec<Order> {
        self.pending.iter().map(|p| p.order.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::candle;
    use crate::paper::{BpsSlippage, FeeAsset, NextOpenFillModel, RateFee};
    use bot_core::margin::Bracket;
    use bot_core::{InstrumentSpec, OrderKind, OrderSpec, Timestamp};
    use rust_decimal_macros::dec;

    fn perp() -> Instrument {
        let spot = crate::paper::test_support::btcusdt();
        Instrument::new(InstrumentSpec {
            class: AssetClass::Perpetual,
            tick_size: Price::new(dec!(0.1)),
            lot_size: Qty::new(dec!(0.001)),
            min_qty: Qty::new(dec!(0.001)),
            ..(*spot).clone()
        })
        .unwrap()
    }

    /// Two tiers: < 50,000 notional 50x / 0.4% / cum 0; above 20x / 1% /
    /// cum 300.
    fn brackets() -> Brackets {
        Brackets::new(vec![
            Bracket {
                floor: Notional::ZERO,
                cap: Notional::new(dec!(50000)),
                max_leverage: 50,
                rate: dec!(0.004),
                cum: dec!(0),
            },
            Bracket {
                floor: Notional::new(dec!(50000)),
                cap: Notional::new(dec!(100000000)),
                max_leverage: 20,
                rate: dec!(0.01),
                cum: dec!(300),
            },
        ])
        .unwrap()
    }

    /// Maker 0.02%, taker 0.05%, no slippage, clearance 1.25%.
    fn broker(wallet: Decimal, leverage: u32, mode: MarginMode) -> PerpPaperBroker {
        let p = perp();
        let terms = HashMap::from([(
            p.id.clone(),
            PerpTerms {
                leverage,
                mode,
                brackets: brackets(),
                close_out: None,
            },
        )]);
        PerpPaperBroker::new(
            vec![p],
            terms,
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(dec!(0)).unwrap()),
            Box::new(RateFee::new(dec!(0.0002), dec!(0.0005), FeeAsset::Quote).unwrap()),
            Notional::new(wallet),
            dec!(0.0125),
        )
        .unwrap()
    }

    fn market(n: u32, side: Side, qty: Decimal, after: i64) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(format!("o-{n}")).unwrap(),
            instrument: perp().id.clone(),
            side,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
            created_at: candle(after, ["1", "1", "1", "1"]).close_time,
        })
        .unwrap()
    }

    async fn bar(b: &mut PerpPaperBroker, i: i64, ohlc: [&str; 4]) -> Vec<Fill> {
        b.settle(&MarketEvent::Candle(candle(i, ohlc)))
            .await
            .unwrap()
    }

    /// Isolated 10x, wallet 10,000. Buy 1 @ 30,000 (fee 15): 3,000 moves
    /// to the position, wallet 6,985. Sell 1 @ 31,000 (fee 15.5): +1,000,
    /// the 3,000 back → wallet 10,969.5 = 10,000 − 15 + 1,000 − 15.5.
    #[tokio::test]
    async fn isolated_round_trip_moves_margin_and_settles_pnl() {
        let mut b = broker(dec!(10000), 10, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        let f = bar(&mut b, 1, ["30000", "31000", "29900", "31000"]).await;
        assert_eq!(f.len(), 1);
        assert_eq!(b.wallet(), Notional::new(dec!(6985)));
        assert_eq!(b.isolated_margin(&perp().id), Notional::new(dec!(3000)));
        b.submit(market(2, Side::Sell, dec!(1), 1)).await.unwrap();
        bar(&mut b, 2, ["31000", "31000", "31000", "31000"]).await;
        assert!(b.position(&perp().id).is_flat());
        assert_eq!(b.wallet(), Notional::new(dec!(10969.5)));
        assert_eq!(b.isolated_margin(&perp().id), Notional::ZERO);
    }

    /// Same long; LP = (3000 − 30000)/(0.004 − 1) = 27,108.43…, sold at
    /// 27,108.4 (tick, against the trader): loss 2,891.6 leaves 108.4 of
    /// margin; 1.25% clearance (338.855) is capped at it → fee 108.4, the
    /// margin is gone, nothing more: wallet stays 6,985. A bar whose low
    /// stays above LP doesn't liquidate.
    #[tokio::test]
    async fn isolated_long_is_liquidated_at_its_price_and_loses_only_its_margin() {
        let mut b = broker(dec!(10000), 10, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        bar(&mut b, 1, ["30000", "30000", "27200", "28000"]).await;
        assert_eq!(
            b.liquidation_price(&perp().id).unwrap().get().round_dp(2),
            dec!(27108.43)
        );
        let f = bar(&mut b, 2, ["28000", "28000", "27000", "27500"]).await;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].order_id.as_str(), "liq-1");
        assert_eq!(
            (f[0].side, f[0].price),
            (Side::Sell, Price::new(dec!(27108.4)))
        );
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(108.4))));
        assert_eq!(f[0].ts, candle(2, ["1", "1", "1", "1"]).close_time);
        assert!(b.position(&perp().id).is_flat());
        assert_eq!(b.wallet(), Notional::new(dec!(6985)));
    }

    /// Same long, LP 27,108.43. Bar 2's trade low 27,000 would breach,
    /// but its mark bar's low is 27,500: no liquidation (the venue checks
    /// mark). Bar 3: trade low 27,200 wouldn't, mark low 27,100 does →
    /// liquidated. A mark bar of another bar time is ignored.
    #[tokio::test]
    async fn liquidation_follows_the_mark_price_bar() {
        let mark = |i: i64, ohlc: [&str; 4]| MarketEvent::Mark(candle(i, ohlc));
        let mut b = broker(dec!(10000), 10, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        bar(&mut b, 1, ["30000", "30000", "29000", "29000"]).await;
        b.settle(&mark(2, ["29000", "29000", "27500", "28000"]))
            .await
            .unwrap();
        assert!(
            bar(&mut b, 2, ["29000", "29000", "27000", "28000"])
                .await
                .is_empty()
        );
        b.settle(&mark(3, ["28000", "28000", "27100", "27900"]))
            .await
            .unwrap();
        let f = bar(&mut b, 3, ["28000", "28000", "27200", "27900"]).await;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].order_id.as_str(), "liq-1");
        // Stale mark bar (bar 3) at bar 4: the trade bar decides again.
        let mut b = broker(dec!(10000), 10, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        b.settle(&mark(0, ["30000", "30000", "29900", "30000"]))
            .await
            .unwrap();
        let f = bar(&mut b, 1, ["30000", "30000", "27000", "28000"]).await;
        assert_eq!(f.len(), 2, "fill + liquidation on the trade bar");
    }

    /// Isolated 5x short 1 @ 30,000 (fee 15): margin 6,000, wallet 3,985.
    /// Funding 0.01% at mark 30,500: the short receives 3.05 into its
    /// margin. Buy back @ 29,000 (fee 14.5): +1,000 → wallet 10,973.55 =
    /// 10,000 − 15 + 3.05 + 1,000 − 14.5.
    #[tokio::test]
    async fn isolated_short_receives_funding_and_profits_on_a_fall() {
        let mut b = broker(dec!(10000), 5, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Sell, dec!(1), 0)).await.unwrap();
        bar(&mut b, 1, ["30000", "30000", "30000", "30000"]).await;
        assert_eq!(b.position(&perp().id).qty(), Qty::new(dec!(-1)));
        assert_eq!(b.wallet(), Notional::new(dec!(3985)));
        b.settle(&MarketEvent::Funding {
            instrument: perp().id.clone(),
            rate: dec!(0.0001),
            mark: Price::new(dec!(30500)),
            ts: Timestamp::from_millis(2 * 3_600_000),
        })
        .await
        .unwrap();
        assert_eq!(b.isolated_margin(&perp().id), Notional::new(dec!(6003.05)));
        b.submit(market(2, Side::Buy, dec!(1), 1)).await.unwrap();
        bar(&mut b, 2, ["29000", "29000", "29000", "29000"]).await;
        assert!(b.position(&perp().id).is_flat());
        assert_eq!(b.wallet(), Notional::new(dec!(10973.55)));
        assert_eq!(
            b.position(&perp().id).realized_pnl(),
            Notional::new(dec!(973.55))
        );
    }

    /// Cross 10x, wallet 5,000: long 1 @ 30,000 → wallet 4,985, LP =
    /// (4985 − 30000)/(0.004 − 1) = 25,115.46…; a low of 25,200 survives,
    /// 25,000 liquidates at 25,115.4: loss 4,884.6 leaves 100.4; clearance
    /// (313.9425) capped at it → fee 100.4, wallet 0.
    #[tokio::test]
    async fn cross_long_is_liquidated_against_the_whole_wallet() {
        let mut b = broker(dec!(5000), 10, MarginMode::Cross);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        bar(&mut b, 1, ["30000", "30000", "29000", "29000"]).await;
        assert_eq!(b.wallet(), Notional::new(dec!(4985)));
        assert!(
            bar(&mut b, 2, ["29000", "29000", "25200", "25300"])
                .await
                .is_empty()
        );
        let f = bar(&mut b, 3, ["25300", "25300", "25000", "25100"]).await;
        assert_eq!(f[0].price, Price::new(dec!(25115.4)));
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(100.4))));
        assert_eq!(b.wallet(), Notional::ZERO);
        assert!(b.position(&perp().id).is_flat());
    }

    /// Isolated 2x, wallet 1,000: long 10 @ 100 (fee 0.5) → margin 500,
    /// wallet 499.5. At 60 (liquidation 50.2, safe), flipping by selling
    /// 30 to short 20 would take 600 margin, get back 500 − 400 loss −
    /// 0.9 fee = 99.1: needs 500.9 > 499.5 → refused (counting only the
    /// growth, 10 × 60 / 2 + fee = 300.9, would have let it through and
    /// left the wallet at −1.4). Selling 29 (short 19) needs 570 − 99.13 =
    /// 470.87 → accepted; filled at 60: wallet 28.63, margin 570.
    #[tokio::test]
    async fn a_losing_flip_needs_margin_net_of_what_the_close_returns() {
        let mut b = broker(dec!(1000), 2, MarginMode::Isolated);
        bar(&mut b, 0, ["100", "100", "100", "100"]).await;
        b.submit(market(1, Side::Buy, dec!(10), 0)).await.unwrap();
        bar(&mut b, 1, ["100", "100", "60", "60"]).await;
        assert_eq!(b.wallet(), Notional::new(dec!(499.5)));
        assert!(b.position(&perp().id).qty().is_positive());
        assert!(b.submit(market(2, Side::Sell, dec!(30), 1)).await.is_err());
        b.submit(market(3, Side::Sell, dec!(29), 1)).await.unwrap();
        bar(&mut b, 2, ["60", "60", "60", "60"]).await;
        assert_eq!(b.position(&perp().id).qty(), Qty::new(dec!(-19)));
        assert_eq!(b.wallet(), Notional::new(dec!(28.63)));
        assert_eq!(b.isolated_margin(&perp().id), Notional::new(dec!(570)));
        // Short 19 @ 60 (liquidation 89.64). At 85, buying 25 flips to a
        // smaller long 6: the close returns 570 − 475 − 1.0625 fee =
        // 93.9375, the long needs 255 → 161.06 > 28.63 → refused, though
        // the position shrinks (19 → 6). Buying 19 only closes: accepted.
        bar(&mut b, 3, ["60", "85", "60", "85"]).await;
        assert!(b.submit(market(4, Side::Buy, dec!(25), 3)).await.is_err());
        assert!(b.submit(market(5, Side::Buy, dec!(19), 3)).await.is_ok());
    }

    /// 3,015 (3,000 margin + 15 fee) needed on 1,000 → refused. At 25x, 1
    /// BTC (30,000 notional, tier 1: 50x) is fine but 2 (60,000, tier 2:
    /// 20x) isn't. Leverage above the first tier's 50x is refused at build.
    #[tokio::test]
    async fn refuses_orders_past_margin_or_bracket_leverage() {
        let mut b = broker(dec!(1000), 10, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        assert!(b.submit(market(1, Side::Buy, dec!(1), 0)).await.is_err());
        let mut b = broker(dec!(100000), 25, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        assert!(b.submit(market(1, Side::Sell, dec!(1), 0)).await.is_ok());
        assert!(b.submit(market(2, Side::Buy, dec!(3), 0)).await.is_err());
        let p = perp();
        let terms = HashMap::from([(
            p.id.clone(),
            PerpTerms {
                leverage: 60,
                mode: MarginMode::Isolated,
                brackets: brackets(),
                close_out: None,
            },
        )]);
        assert!(
            PerpPaperBroker::new(
                vec![p],
                terms,
                Box::new(NextOpenFillModel),
                Box::new(BpsSlippage::new(dec!(0)).unwrap()),
                Box::new(RateFee::new(dec!(0), dec!(0), FeeAsset::Quote).unwrap()),
                Notional::new(dec!(1000)),
                dec!(0.0125),
            )
            .is_err()
        );
    }

    /// Long 1. A reduce-only sell of 2 would flip: refused; a reduce-only
    /// buy adds to the long: refused. A plain sell of 0.6 and a reduce-only
    /// sell of 1, both filled at the next open in that order: the second
    /// is clipped to the 0.4 left → flat, never short.
    #[tokio::test]
    async fn reduce_only_never_opens_or_flips() {
        let ro = |n: u32, side: Side, q: Decimal| market(n, side, q, 1).with_reduce_only().unwrap();
        let mut b = broker(dec!(100000), 5, MarginMode::Isolated);
        bar(&mut b, 0, ["30000", "30000", "30000", "30000"]).await;
        b.submit(market(1, Side::Buy, dec!(1), 0)).await.unwrap();
        bar(&mut b, 1, ["30000", "30000", "30000", "30000"]).await;
        assert!(b.submit(ro(2, Side::Sell, dec!(2))).await.is_err());
        assert!(b.submit(ro(3, Side::Buy, dec!(0.5))).await.is_err());
        b.submit(market(4, Side::Sell, dec!(0.6), 1)).await.unwrap();
        b.submit(ro(5, Side::Sell, dec!(1))).await.unwrap();
        let f = bar(&mut b, 2, ["30000", "30000", "30000", "30000"]).await;
        let sold: Vec<Decimal> = f.iter().map(|f| f.qty.get()).collect();
        assert_eq!(sold, vec![dec!(0.6), dec!(0.4)]);
        assert!(b.position(&perp().id).is_flat());
    }

    /// A CFD on the test id, tick 0.01, unit 0.001.
    fn cfd() -> Instrument {
        Instrument::new(InstrumentSpec {
            class: AssetClass::Cfd,
            tick_size: Price::new(dec!(0.01)),
            ..(*perp()).clone()
        })
        .unwrap()
    }

    /// eToro-like: 0.15% each way, no slippage, `leverage`, `wallet`.
    fn cfd_broker(wallet: Decimal, leverage: u32) -> PerpPaperBroker {
        let i = cfd();
        PerpPaperBroker::new(
            vec![i.clone()],
            HashMap::from([(i.id.clone(), PerpTerms::cfd(leverage).unwrap())]),
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(dec!(0)).unwrap()),
            Box::new(RateFee::new(dec!(0.0015), dec!(0.0015), FeeAsset::Quote).unwrap()),
            Notional::new(wallet),
            dec!(0),
        )
        .unwrap()
    }

    /// Short 10 at 100, 1x: margin 1,000 + fee 1.50 → wallet 998.50;
    /// close-out at 150. High 149: nothing. A bar opening at 140 and
    /// reaching 151 closes it at 150: loss 500, fee 2.25 → 998.50 + 1,000
    /// − 500 − 2.25 = 1,496.25.
    #[tokio::test]
    async fn a_cfd_short_is_closed_out_at_half_its_margin() {
        let mut b = cfd_broker(dec!(2000), 1);
        bar(&mut b, 0, ["100", "100", "100", "100"]).await;
        b.submit(market(1, Side::Sell, dec!(10), 0)).await.unwrap();
        bar(&mut b, 1, ["100", "100", "100", "100"]).await;
        assert_eq!(b.wallet(), Notional::new(dec!(998.50)));
        assert_eq!(b.liquidation_price(&cfd().id), Some(Price::new(dec!(150))));
        assert!(
            bar(&mut b, 2, ["120", "149", "119", "140"])
                .await
                .is_empty()
        );
        let f = bar(&mut b, 3, ["140", "151", "139", "145"]).await;
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].side, f[0].price), (Side::Buy, Price::new(dec!(150))));
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(2.25))));
        assert!(b.position(&cfd().id).is_flat());
        assert_eq!(b.wallet(), Notional::new(dec!(1496.25)));
    }

    /// Same short; the bar opens at 160, past the close-out: filled at the
    /// open, loss 600, fee 2.40 → 998.50 + 1,000 − 600 − 2.40 = 1,396.10.
    #[tokio::test]
    async fn a_gap_past_the_close_out_fills_at_the_open() {
        let mut b = cfd_broker(dec!(2000), 1);
        bar(&mut b, 0, ["100", "100", "100", "100"]).await;
        b.submit(market(1, Side::Sell, dec!(10), 0)).await.unwrap();
        bar(&mut b, 1, ["100", "100", "100", "100"]).await;
        let f = bar(&mut b, 2, ["160", "165", "158", "160"]).await;
        assert_eq!(f[0].price, Price::new(dec!(160)));
        assert_eq!(b.wallet(), Notional::new(dec!(1396.10)));
    }

    /// 5x: margin 200 + fee 1.50 from 300 → 98.50; close-out at 110. A gap
    /// to 200 loses 1,000 + fee 3.00, past the margin: the account pays
    /// (98.50 + 200 − 1,003 = −704.50) and floors at zero.
    #[tokio::test]
    async fn a_gap_can_cost_more_than_the_margin_but_not_below_zero() {
        let mut b = cfd_broker(dec!(300), 5);
        bar(&mut b, 0, ["100", "100", "100", "100"]).await;
        b.submit(market(1, Side::Sell, dec!(10), 0)).await.unwrap();
        bar(&mut b, 1, ["100", "100", "100", "100"]).await;
        assert_eq!(b.wallet(), Notional::new(dec!(98.50)));
        assert_eq!(b.liquidation_price(&cfd().id), Some(Price::new(dec!(110))));
        let f = bar(&mut b, 2, ["200", "200", "200", "200"]).await;
        assert_eq!(f[0].fee, Fee::Quote(Notional::new(dec!(3.00))));
        assert!(b.position(&cfd().id).is_flat());
        assert_eq!(b.wallet(), Notional::ZERO);
    }

    /// A CFD can't be given bracket liquidation or cross margin.
    #[test]
    fn a_cfd_needs_its_close_out_terms() {
        let i = cfd();
        let mut terms = PerpTerms::cfd(2).unwrap();
        terms.close_out = None;
        let r = PerpPaperBroker::new(
            vec![i.clone()],
            HashMap::from([(i.id.clone(), terms)]),
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(dec!(0)).unwrap()),
            Box::new(RateFee::new(dec!(0.0015), dec!(0.0015), FeeAsset::Quote).unwrap()),
            Notional::new(dec!(100)),
            dec!(0),
        );
        assert!(r.is_err());
    }
}
