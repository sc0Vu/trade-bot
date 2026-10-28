use super::{Decision, Execution, FeeModel, FillModel, OrderState, SlippageModel};
use crate::{Broker, ExchangeError};
use async_trait::async_trait;
use bot_core::{
    Asset, Candle, Fee, Fill, FillSpec, Instrument, InstrumentId, Liquidity, MarketEvent, Notional,
    Order, OrderId, OrderKind, Position, Price, Qty, Side, Size,
};
use std::collections::HashMap;
use std::sync::Arc;

/// What a venue currently lets an account do on a market, consulted by the
/// paper broker at submit and cancel. Live venues refuse orders on a halted
/// market; a paper run on that venue's data must too (invariant 3).
pub trait OrderGate: Send + Sync {
    /// `Err(reason)` if the venue would refuse a new order on `id` now.
    fn check_submit(&self, id: &InstrumentId) -> Result<(), &'static str>;
    /// `Err(reason)` if the venue would refuse cancelling an order on `id`.
    fn check_cancel(&self, id: &InstrumentId) -> Result<(), &'static str>;
}

struct Pending {
    order: Order,
    state: OrderState,
    /// Quote held back for this buy (zero for sells): the most it can
    /// spend, set at submit.
    reserved: Notional,
}

/// The one quote balance the broker trades from.
struct Cash {
    asset: Asset,
    balance: Notional,
}

/// Simulated broker for backtest and paper trading.
///
/// Orders wait until a candle that opened after they were created, then the
/// [`FillModel`] decides; the [`SlippageModel`] and [`FeeModel`] set the
/// final price and fee, and the fill is applied to the position with
/// `Position::apply`. Pending orders are settled in submission order.
///
/// A quote-budget market buy (`Size::Quote`) is sized at the fill: the
/// largest lot multiple whose notional plus any quote-denominated fee fits
/// the budget, at the slipped price. Spend never exceeds the budget. If
/// that size breaks a venue filter the order is dropped unfilled.
///
/// Rejects at submit what a spot venue would: unknown instruments, reused
/// ids, and sells larger than the holding minus sells already pending.
///
/// Given a cash balance ([`PaperBroker::with_cash`]) it also refuses buys
/// it can't pay for, at submit like Binance (`-2010`). Each pending buy
/// reserves the most it can spend: a quote budget its budget; a limit or
/// stop-limit its notional at the limit plus the larger of the maker and
/// taker quote fee; a market or stop-market buy by quantity its notional
/// at the last close (the stop, if higher), slipped, plus the taker fee.
/// Available = balance − reservations + the expected proceeds of pending
/// market sells (last close, slipped, less fee): live, a market sell fills
/// at once, so a buy placed right after can spend what it raised, and
/// rotating strategies depend on that. Resting limit sells don't count;
/// they may never fill. When a buy fills, its actual cost is checked
/// again against what is then available (an expected sale may have
/// raised less, or been cancelled) and the buy is dropped unfilled if it
/// no longer fits, so cash never goes negative. Orders on an instrument
/// quoted in another asset are refused. Without a balance nothing is
/// refused for lack of quote.
///
/// Given an [`OrderGate`] ([`PaperBroker::with_gate`]) it also refuses what
/// the gate says the venue would (a halted market). The gate decides only
/// submit and cancel: orders already resting keep settling.
pub struct PaperBroker {
    instruments: HashMap<InstrumentId, Instrument>,
    fill_model: Box<dyn FillModel>,
    slippage: Box<dyn SlippageModel>,
    fees: Box<dyn FeeModel>,
    pending: Vec<Pending>,
    positions: HashMap<InstrumentId, Position>,
    cash: Option<Cash>,
    /// Latest close per instrument: prices market orders for the balance
    /// check.
    last_close: HashMap<InstrumentId, Price>,
    gate: Option<Arc<dyn OrderGate>>,
}

impl PaperBroker {
    /// A broker for `instruments` (the resolved universe) with the given
    /// models.
    pub fn new(
        instruments: Vec<Instrument>,
        fill_model: Box<dyn FillModel>,
        slippage: Box<dyn SlippageModel>,
        fees: Box<dyn FeeModel>,
    ) -> Self {
        Self {
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            fill_model,
            slippage,
            fees,
            pending: Vec::new(),
            positions: HashMap::new(),
            cash: None,
            last_close: HashMap::new(),
            gate: None,
        }
    }

    /// Also refuses submits and cancels `gate` says the venue would.
    pub fn with_gate(mut self, gate: Arc<dyn OrderGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Trades from a balance of `balance` in `asset` (the quote of the
    /// universe), refusing buys it can't cover. See the type docs.
    pub fn with_cash(mut self, asset: Asset, balance: Notional) -> Self {
        self.cash = Some(Cash { asset, balance });
        self
    }

    /// Current quote balance, fills applied; `None` without one.
    pub fn cash(&self) -> Option<Notional> {
        self.cash.as_ref().map(|c| c.balance)
    }

    /// The larger of the maker and taker fee on a buy, where it is paid in
    /// quote; zero where it is taken from the base received.
    fn worst_quote_fee(&self, instrument: &Instrument, q: Qty, price: Price) -> Notional {
        [Liquidity::Maker, Liquidity::Taker]
            .into_iter()
            .map(|l| quote_part(&self.fees.fee(instrument, Side::Buy, q, price, l)))
            .max()
            .unwrap_or(Notional::ZERO)
    }

    /// `price` worsened by taker slippage for `side`.
    fn slipped(&self, instrument: &Instrument, side: Side, price: Price) -> Price {
        self.slippage.apply(
            instrument,
            side,
            Execution {
                price,
                liquidity: Liquidity::Taker,
                at_open: true,
            },
        )
    }

    /// Most quote `order` can spend; zero for a sell.
    fn reservation(&self, order: &Order) -> Result<Notional, ExchangeError> {
        let Some(cash) = &self.cash else {
            return Ok(Notional::ZERO);
        };
        let instrument = self
            .instruments
            .get(&order.instrument)
            .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?;
        if instrument.quote != cash.asset {
            return Err(ExchangeError::Rejected(
                "no balance in the order's quote asset",
            ));
        }
        if order.side == Side::Sell {
            return Ok(Notional::ZERO);
        }
        let qty = match order.size {
            Size::Quote(budget) => return Ok(budget),
            Size::Base(q) => q,
        };
        let price = match order.kind {
            OrderKind::Limit { price, .. } => price,
            OrderKind::StopLimit { limit, .. } => limit,
            OrderKind::Market | OrderKind::StopMarket { .. } => {
                let last = *self
                    .last_close
                    .get(&order.instrument)
                    .ok_or(ExchangeError::Rejected("no price yet to check the balance"))?;
                let base = match order.kind {
                    OrderKind::StopMarket { stop } => last.max(stop),
                    _ => last,
                };
                self.slipped(instrument, Side::Buy, base)
            }
        };
        Ok(instrument.notional(qty, price) + self.worst_quote_fee(instrument, qty, price))
    }

    /// Quote a new buy may spend now: balance, less pending reservations,
    /// plus the expected proceeds of pending market sells.
    fn available(&self) -> Notional {
        let Some(cash) = &self.cash else {
            return Notional::ZERO;
        };
        let mut free = cash.balance;
        for p in &self.pending {
            free -= p.reserved;
            if p.order.side == Side::Sell && p.order.kind == OrderKind::Market {
                free += self.expected_proceeds(&p.order);
            }
        }
        free
    }

    /// What a pending market sell should raise at the last close, slipped,
    /// less a quote fee; zero if there is no close to price it.
    fn expected_proceeds(&self, order: &Order) -> Notional {
        let (Some(instrument), Some(last), Size::Base(q)) = (
            self.instruments.get(&order.instrument),
            self.last_close.get(&order.instrument),
            order.size,
        ) else {
            return Notional::ZERO;
        };
        let price = self.slipped(instrument, Side::Sell, *last);
        let fee = quote_part(
            &self
                .fees
                .fee(instrument, Side::Sell, q, price, Liquidity::Taker),
        );
        instrument.notional(q, price) - fee
    }

    fn pending_sell_qty(&self, id: &InstrumentId) -> Qty {
        self.pending
            .iter()
            .filter(|p| p.order.instrument == *id && p.order.side == Side::Sell)
            .map(|p| match p.order.size {
                Size::Base(q) => q,
                // Quote size is buy-only (Order::new), so never reached.
                Size::Quote(_) => Qty::ZERO,
            })
            .sum()
    }

    /// Largest lot multiple buyable with `budget` at `price`, counting a
    /// quote-denominated fee as spend. `None` if it breaks a venue filter.
    fn budget_qty(
        &self,
        instrument: &Instrument,
        budget: Notional,
        price: Price,
        liquidity: Liquidity,
    ) -> Option<Qty> {
        let cost = |q: Qty| {
            let notional = instrument.notional(q, price);
            match self.fees.fee(instrument, Side::Buy, q, price, liquidity) {
                Fee::Quote(f) => notional + f,
                Fee::Base(_) | Fee::Other { .. } => notional,
            }
        };
        let mut q = instrument.round_qty(instrument.qty_for(budget, price).ok()?);
        let first = cost(q);
        if first > budget {
            // Shrink by the effective fee rate, then step down lot by lot.
            q = instrument.round_qty(q * (budget / first));
        }
        while q.is_positive() && cost(q) > budget {
            q -= instrument.lot_size;
        }
        instrument.check_order(q, price).ok()?;
        Some(q)
    }

    fn execute(&mut self, order: &Order, candle: &Candle, exec: super::Execution) -> Option<Fill> {
        let instrument = self.instruments.get(&order.instrument)?;
        // A limit never fills worse than its limit: slippage on a taker
        // limit fill (marketable at the open) stops at the limit price.
        let price = cap_at_limit(order, self.slippage.apply(instrument, order.side, exec));
        let qty = match order.size {
            Size::Base(q) => q,
            Size::Quote(budget) => match self.budget_qty(instrument, budget, price, exec.liquidity)
            {
                Some(q) => q,
                None => {
                    tracing::warn!(order = %order.id, "quote budget too small at fill price; order dropped");
                    return None;
                }
            },
        };
        let fee = self
            .fees
            .fee(instrument, order.side, qty, price, exec.liquidity);
        let ts = if exec.at_open {
            candle.open_time
        } else {
            candle.close_time
        };
        let fill = match Fill::new(FillSpec {
            order_id: order.id.clone(),
            instrument: order.instrument.clone(),
            side: order.side,
            qty,
            price,
            fee,
            liquidity: exec.liquidity,
            ts,
        }) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(order = %order.id, error = %e, "paper fill invalid; order dropped");
                return None;
            }
        };
        let notional = instrument.notional(fill.qty, fill.price);
        let quote_fee = quote_part(&fill.fee);
        if order.side == Side::Buy && self.cash.is_some() && notional + quote_fee > self.available()
        {
            tracing::warn!(order = %order.id, "insufficient balance at fill; order dropped");
            return None;
        }
        let position = self
            .positions
            .entry(order.instrument.clone())
            .or_insert_with(|| Position::flat(order.instrument.clone()));
        if let Err(e) = position.apply(&fill, instrument) {
            tracing::warn!(order = %order.id, error = %e, "paper fill rejected by position; order dropped");
            return None;
        }
        if let Some(cash) = &mut self.cash {
            match order.side {
                Side::Buy => cash.balance -= notional + quote_fee,
                Side::Sell => cash.balance += notional - quote_fee,
            }
        }
        Some(fill)
    }
}

/// The part of a fee paid in quote: a quote fee, or a third-asset fee
/// valued in quote (as position accounting counts it).
fn quote_part(fee: &Fee) -> Notional {
    match fee {
        Fee::Quote(n) | Fee::Other { value: Some(n), .. } => *n,
        Fee::Base(_) | Fee::Other { value: None, .. } => Notional::ZERO,
    }
}

fn cap_at_limit(order: &Order, price: Price) -> Price {
    let limit = match order.kind {
        OrderKind::Limit { price, .. } => price,
        OrderKind::StopLimit { limit, .. } => limit,
        OrderKind::Market | OrderKind::StopMarket { .. } => return price,
    };
    match order.side {
        Side::Buy => price.min(limit),
        Side::Sell => price.max(limit),
    }
}

#[async_trait]
impl Broker for PaperBroker {
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError> {
        if !self.instruments.contains_key(&order.instrument) {
            return Err(ExchangeError::UnknownInstrument(order.instrument.clone()));
        }
        if self.pending.iter().any(|p| p.order.id == order.id) {
            return Err(ExchangeError::DuplicateOrder(order.id.clone()));
        }
        if let Some(g) = &self.gate {
            g.check_submit(&order.instrument)
                .map_err(ExchangeError::Rejected)?;
        }
        crate::check_open_count(
            self.instruments
                .get(&order.instrument)
                .ok_or_else(|| ExchangeError::UnknownInstrument(order.instrument.clone()))?,
            self.pending.iter().map(|p| &p.order),
            &order,
        )?;
        if let (Side::Sell, Size::Base(qty)) = (order.side, order.size) {
            let held = self.position(&order.instrument).qty();
            if self.pending_sell_qty(&order.instrument) + qty > held {
                return Err(ExchangeError::Rejected("sell exceeds holding"));
            }
        }
        let reserved = self.reservation(&order)?;
        if reserved > self.available() {
            return Err(ExchangeError::Rejected("insufficient balance"));
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
        if let (Some(g), Some(p)) = (&self.gate, self.pending.iter().find(|p| p.order.id == id)) {
            g.check_cancel(&p.order.instrument)
                .map_err(ExchangeError::Rejected)?;
        }
        let before = self.pending.len();
        self.pending.retain(|p| p.order.id != id);
        if self.pending.len() == before {
            return Err(ExchangeError::UnknownOrder(id));
        }
        Ok(())
    }

    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        let MarketEvent::Candle(candle) = event else {
            return Ok(Vec::new());
        };
        let mut fills = Vec::new();
        // In place, so a filling order is checked against the orders still
        // pending around it.
        let mut i = 0;
        while i < self.pending.len() {
            let p = &self.pending[i];
            // Only candles that opened after the order was decided: the bar
            // it was decided on (or any earlier one) can't fill it.
            if p.order.instrument != candle.instrument || candle.open_time <= p.order.created_at {
                i += 1;
                continue;
            }
            match self.fill_model.evaluate(&p.order, p.state, candle) {
                Decision::Fill(exec) => {
                    let p = self.pending.remove(i);
                    fills.extend(self.execute(&p.order, candle, exec));
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
        self.last_close
            .insert(candle.instrument.clone(), candle.close);
        Ok(fills)
    }

    fn position(&self, instrument: &InstrumentId) -> Position {
        self.positions
            .get(instrument)
            .cloned()
            .unwrap_or_else(|| Position::flat(instrument.clone()))
    }

    fn open_orders(&self) -> Vec<Order> {
        self.pending.iter().map(|p| p.order.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::{HOUR, btcusdt, candle};
    use crate::paper::{BpsSlippage, FeeAsset, NextOpenFillModel, RateFee};
    use bot_core::{
        Fee, InstrumentSpec, Liquidity, Notional, OrderKind, OrderSpec, Price, Timestamp,
    };
    use rust_decimal_macros::dec;

    fn broker(slippage_bps: rust_decimal::Decimal) -> PaperBroker {
        PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(slippage_bps).unwrap()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap()),
        )
    }

    /// Market order decided at the close of candle `i`.
    fn market(id: &str, side: Side, qty: rust_decimal::Decimal, decided_on: i64) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(decided_on * HOUR + HOUR - 1),
        })
        .unwrap()
    }

    fn ev(i: i64, ohlc: [&str; 4]) -> MarketEvent {
        MarketEvent::Candle(candle(i, ohlc))
    }

    #[tokio::test]
    async fn never_fills_on_the_bar_it_was_decided_on() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        let fills = b.settle(&ev(0, ["100", "101", "99", "100"])).await.unwrap();
        assert!(fills.is_empty());
        assert_eq!(b.open_orders().len(), 1);
    }

    /// Buy 10 @ next open 101, no slippage, fee 0.1% in base:
    /// fee 0.01 BTC, position 9.99 BTC at cost 1010 USDT.
    #[tokio::test]
    async fn market_buy_fills_at_next_open() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        let fills = b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap();

        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.price, Price::new(dec!(101)));
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.01))));
        assert_eq!(f.liquidity, Liquidity::Taker);
        assert_eq!(f.ts, Timestamp::from_millis(HOUR));

        let pos = b.position(&btcusdt().id);
        assert_eq!(pos.qty(), Qty::new(dec!(9.99)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(1010)));
        assert_eq!(b.open_orders().len(), 0);
    }

    /// Same with 10 bps: 101 * 1.001 = 101.101, up to tick 101.11;
    /// cost 10 * 101.11 = 1011.1.
    #[tokio::test]
    async fn slippage_applies_to_market_fill() {
        let mut b = broker(dec!(10));
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        let fills = b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap();
        assert_eq!(fills[0].price, Price::new(dec!(101.11)));
        assert_eq!(
            b.position(&btcusdt().id).cost_basis(),
            Notional::new(dec!(1011.1))
        );
    }

    fn budget_buy(budget: rust_decimal::Decimal) -> Order {
        Order::new(OrderSpec {
            size: Size::Quote(Notional::new(budget)),
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap()
    }

    /// Budget 1000 at open 101, fee 0.1% in base:
    /// 1000 / 101 = 9.90099009.. -> lot 9.90099; spend 9.90099 * 101 =
    /// 999.99999 <= 1000; fee 0.0099009900 BTC.
    #[tokio::test]
    async fn quote_budget_sized_at_fill_price() {
        let mut b = broker(dec!(0));
        b.submit(budget_buy(dec!(1000))).await.unwrap();
        let f = &b
            .settle(&ev(1, ["101", "103", "100", "102"]))
            .await
            .unwrap()[0];
        assert_eq!(f.qty, Qty::new(dec!(9.90099)));
        assert_eq!(
            btcusdt().notional(f.qty, f.price),
            Notional::new(dec!(999.99999))
        );
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.0099009900))));
    }

    /// Budget 1000 at open 100, fee 0.1% in quote. 10 BTC would cost
    /// 1000 + 1 fee = 1001. Largest fitting lot multiple: 9.99 BTC ->
    /// 999 + 0.999 = 999.999; one more lot (9.99001) costs 1000.000001.
    #[tokio::test]
    async fn quote_budget_counts_quote_fee_as_spend() {
        let mut b = PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::zero()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Quote).unwrap()),
        );
        b.submit(budget_buy(dec!(1000))).await.unwrap();
        let f = &b
            .settle(&ev(1, ["100", "100", "100", "100"]))
            .await
            .unwrap()[0];
        assert_eq!(f.qty, Qty::new(dec!(9.99)));
        assert_eq!(f.fee, Fee::Quote(Notional::new(dec!(0.999))));
    }

    #[tokio::test]
    async fn budget_below_min_notional_is_dropped() {
        let mut b = broker(dec!(0));
        b.submit(budget_buy(dec!(4))).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "100", "100", "100"]))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(b.position(&btcusdt().id).is_flat());
        assert_eq!(b.open_orders().len(), 0);
    }

    #[tokio::test]
    async fn rejects_what_a_spot_venue_would() {
        let mut b = broker(dec!(0));
        // Nothing held: can't sell.
        assert!(matches!(
            b.submit(market("o-1", Side::Sell, dec!(1), 0)).await,
            Err(ExchangeError::Rejected(_))
        ));
        let other = Order::new(OrderSpec {
            instrument: "binance:ETHUSDT".parse().unwrap(),
            ..market("o-2", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        assert!(matches!(
            b.submit(other).await,
            Err(ExchangeError::UnknownInstrument(_))
        ));

        b.submit(market("o-3", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        assert!(matches!(
            b.submit(market("o-3", Side::Buy, dec!(1), 0)).await,
            Err(ExchangeError::DuplicateOrder(_))
        ));
    }

    #[tokio::test]
    async fn pending_sells_reserve_the_holding() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        b.settle(&ev(1, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        // Holding 0.999 BTC after the base fee.
        b.submit(market("o-2", Side::Sell, dec!(0.5), 1))
            .await
            .unwrap();
        assert!(
            b.submit(market("o-3", Side::Sell, dec!(0.5), 1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancel_removes_open_order() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "100", "100", "100"]))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            b.cancel(OrderId::new("o-1").unwrap()).await,
            Err(ExchangeError::UnknownOrder(_))
        ));
    }

    #[tokio::test]
    async fn resting_limit_waits_then_fills() {
        let mut b = broker(dec!(10));
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(95)),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        assert!(
            b.settle(&ev(1, ["100", "101", "96", "97"]))
                .await
                .unwrap()
                .is_empty()
        );
        let fills = b.settle(&ev(2, ["97", "98", "94", "96"])).await.unwrap();
        // Maker at the limit; slippage not applied to maker fills.
        assert_eq!(fills[0].price, Price::new(dec!(95)));
        assert_eq!(fills[0].liquidity, Liquidity::Maker);
        assert_eq!(fills[0].ts, Timestamp::from_millis(3 * HOUR - 1));
    }

    /// Buy limit 101, marketable at open 100.95 (taker). 10 bps slippage
    /// would make it 100.95 * 1.001 = 101.05095 -> 101.06, above the
    /// limit; the fill is capped at 101.
    #[tokio::test]
    async fn slippage_never_pushes_a_limit_past_its_price() {
        let mut b = broker(dec!(10));
        let limit = Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(dec!(101)),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..market("o-1", Side::Buy, dec!(1), 0).into()
        })
        .unwrap();
        b.submit(limit).await.unwrap();
        let fills = b
            .settle(&ev(1, ["100.95", "101", "100.90", "101"]))
            .await
            .unwrap();
        assert_eq!(fills[0].liquidity, Liquidity::Taker);
        assert_eq!(fills[0].price, Price::new(dec!(101)));
    }

    fn cashed(balance: rust_decimal::Decimal, slippage_bps: rust_decimal::Decimal) -> PaperBroker {
        broker(slippage_bps).with_cash(btcusdt().quote.clone(), Notional::new(balance))
    }

    fn buy_with(id: &str, budget: rust_decimal::Decimal, decided_on: i64) -> Order {
        Order::new(OrderSpec {
            size: Size::Quote(Notional::new(budget)),
            ..market(id, Side::Buy, dec!(1), decided_on).into()
        })
        .unwrap()
    }

    fn limit_buy(id: &str, qty: rust_decimal::Decimal, price: rust_decimal::Decimal) -> Order {
        Order::new(OrderSpec {
            kind: OrderKind::Limit {
                price: Price::new(price),
                tif: bot_core::TimeInForce::Gtc,
            },
            ..market(id, Side::Buy, qty, 0).into()
        })
        .unwrap()
    }

    fn rejected(r: Result<OrderId, ExchangeError>) -> bool {
        matches!(r, Err(ExchangeError::Rejected(_)))
    }

    #[tokio::test]
    async fn refuses_a_budget_beyond_the_balance() {
        let mut b = cashed(dec!(1000), dec!(0));
        assert!(rejected(b.submit(buy_with("o-1", dec!(1000.01), 0)).await));
        b.submit(buy_with("o-2", dec!(1000), 0)).await.unwrap();
        // The whole balance is now reserved.
        assert!(rejected(b.submit(buy_with("o-3", dec!(5), 0)).await));
    }

    /// Fee 0.1% in quote. Limit 9 @ 100 reserves 900 + 0.9 = 900.9; 1 @
    /// 100 more would need 100.1, total 1001 > 1000. Cancelling the first
    /// frees its reservation.
    #[tokio::test]
    async fn resting_limit_buys_reserve_their_cost() {
        let mut b = PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::zero()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Quote).unwrap()),
        )
        .with_cash(btcusdt().quote.clone(), Notional::new(dec!(1000)));
        b.submit(limit_buy("o-1", dec!(9), dec!(100)))
            .await
            .unwrap();
        assert!(rejected(
            b.submit(limit_buy("o-2", dec!(1), dec!(100))).await
        ));
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        b.submit(limit_buy("o-2", dec!(1), dec!(100)))
            .await
            .unwrap();
    }

    /// Cash 2000, fee 0.1% in the received asset. Buy 10 @ 101 (base fee):
    /// cash 2000 - 1010 = 990. Sell 5 @ 102: 510 less quote fee 0.51 =
    /// 509.49; cash 1499.49.
    #[tokio::test]
    async fn fills_move_the_balance() {
        let mut b = cashed(dec!(2000), dec!(0));
        b.settle(&ev(0, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        b.settle(&ev(1, ["101", "101", "101", "101"]))
            .await
            .unwrap();
        assert_eq!(b.cash(), Some(Notional::new(dec!(990))));
        b.submit(market("o-2", Side::Sell, dec!(5), 1))
            .await
            .unwrap();
        b.settle(&ev(2, ["102", "102", "102", "102"]))
            .await
            .unwrap();
        assert_eq!(b.cash(), Some(Notional::new(dec!(1499.49))));
    }

    /// Holding 9.99 BTC (bought 10 @ 101, base fee) with cash 990, last
    /// close 102. A 1500 buy alone is refused. After a market sell of the
    /// holding, it fits: 990 + 9.99 * 102 = 1018.98 less fee 1.01898 =
    /// 2007.96102 available. At the next open (102) the sale lands first,
    /// then the buy: 1500 / 102 -> lot 14.70588, spend 1499.99976; cash
    /// 2007.96102 - 1499.99976 = 507.96126.
    #[tokio::test]
    async fn a_same_bar_market_sale_can_fund_a_buy() {
        let mut b = cashed(dec!(2000), dec!(0));
        b.settle(&ev(0, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        b.settle(&ev(1, ["101", "102", "101", "102"]))
            .await
            .unwrap();
        assert!(rejected(b.submit(buy_with("o-2", dec!(1500), 1)).await));
        b.submit(market("o-3", Side::Sell, dec!(9.99), 1))
            .await
            .unwrap();
        b.submit(buy_with("o-4", dec!(1500), 1)).await.unwrap();
        let fills = b
            .settle(&ev(2, ["102", "102", "102", "102"]))
            .await
            .unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[1].qty, Qty::new(dec!(14.70588)));
        assert_eq!(b.cash(), Some(Notional::new(dec!(507.96126))));
    }

    /// As above, but the sale is cancelled before the bar: the buy no
    /// longer fits at its fill (1499.99976 > 990) and is dropped unfilled.
    #[tokio::test]
    async fn a_buy_that_no_longer_fits_at_its_fill_is_dropped() {
        let mut b = cashed(dec!(2000), dec!(0));
        b.settle(&ev(0, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        b.submit(market("o-1", Side::Buy, dec!(10), 0))
            .await
            .unwrap();
        b.settle(&ev(1, ["101", "102", "101", "102"]))
            .await
            .unwrap();
        b.submit(market("o-3", Side::Sell, dec!(9.99), 1))
            .await
            .unwrap();
        b.submit(buy_with("o-4", dec!(1500), 1)).await.unwrap();
        b.cancel(OrderId::new("o-3").unwrap()).await.unwrap();
        let fills = b
            .settle(&ev(2, ["102", "102", "102", "102"]))
            .await
            .unwrap();
        assert!(fills.is_empty());
        assert_eq!(b.cash(), Some(Notional::new(dec!(990))));
        assert!(b.open_orders().is_empty());
    }

    /// A market buy by quantity is priced at the last close, slipped: 10
    /// bps on 100 = 100.1 for 1 BTC (fee in base). Before any close it
    /// can't be priced and is refused.
    #[tokio::test]
    async fn a_market_buy_by_quantity_reserves_the_slipped_close() {
        let mut short = cashed(dec!(100.09), dec!(10));
        assert!(rejected(
            short.submit(market("o-1", Side::Buy, dec!(1), 0)).await
        ));
        short
            .settle(&ev(0, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        assert!(rejected(
            short.submit(market("o-1", Side::Buy, dec!(1), 0)).await
        ));

        let mut enough = cashed(dec!(100.1), dec!(10));
        enough
            .settle(&ev(0, ["100", "100", "100", "100"]))
            .await
            .unwrap();
        enough
            .submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refuses_orders_quoted_in_another_asset() {
        let mut b = broker(dec!(0)).with_cash(
            bot_core::Asset::new("BTC").unwrap(),
            Notional::new(dec!(1000)),
        );
        assert!(rejected(b.submit(buy_with("o-1", dec!(10), 0)).await));
    }

    #[track_caller]
    fn assert_rejected(r: Result<OrderId, ExchangeError>, why: &str) {
        match r {
            Err(ExchangeError::Rejected(m)) => assert_eq!(m, why),
            other => panic!("expected refusal {why:?}, got {other:?}"),
        }
    }

    /// Caps of 3 open orders, 1 of them a stop. Buys at a limit far below
    /// the market and stops far above it never fill, so only submit and
    /// cancel move the counts.
    #[tokio::test]
    async fn refuses_past_the_open_order_caps() {
        let capped = Instrument::new(InstrumentSpec {
            max_open_orders: Some(3),
            max_open_stops: Some(1),
            ..btcusdt().into()
        })
        .unwrap();
        let mut b = PaperBroker::new(
            vec![capped],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(dec!(0)).unwrap()),
            Box::new(RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap()),
        );
        let with_kind = |id: &str, kind: OrderKind| {
            Order::new(OrderSpec {
                kind,
                ..market(id, Side::Buy, dec!(0.001), 0).into()
            })
            .unwrap()
        };
        let limit = |id: &str| {
            with_kind(
                id,
                OrderKind::Limit {
                    price: Price::new(dec!(10)),
                    tif: bot_core::TimeInForce::Gtc,
                },
            )
        };
        let stop = |id: &str| {
            with_kind(
                id,
                OrderKind::StopMarket {
                    stop: Price::new(dec!(1000000)),
                },
            )
        };
        for id in ["o-1", "o-2", "o-3"] {
            b.submit(limit(id)).await.unwrap();
        }
        assert_rejected(
            b.submit(limit("o-4")).await,
            "too many open orders on the instrument",
        );
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        b.submit(limit("o-4")).await.unwrap();

        b.cancel(OrderId::new("o-2").unwrap()).await.unwrap();
        b.submit(stop("s-1")).await.unwrap();
        b.cancel(OrderId::new("o-3").unwrap()).await.unwrap();
        assert_rejected(
            b.submit(stop("s-2")).await,
            "too many open stop orders on the instrument",
        );
        // The stop cap leaves room for a plain order.
        b.submit(limit("o-5")).await.unwrap();
        assert_eq!(b.open_orders().len(), 3);
    }

    /// A market halted for new orders, cancels still allowed.
    struct CancelOnly;

    impl OrderGate for CancelOnly {
        fn check_submit(&self, _: &InstrumentId) -> Result<(), &'static str> {
            Err("market is cancel-only")
        }
        fn check_cancel(&self, _: &InstrumentId) -> Result<(), &'static str> {
            Ok(())
        }
    }

    /// Placed before the halt, the order can still be cancelled; nothing new
    /// is accepted, and the refusal is a non-fatal rejection.
    #[tokio::test]
    async fn gate_refuses_what_the_venue_would() {
        let mut b = broker(dec!(0));
        b.submit(market("o-1", Side::Buy, dec!(1), 0))
            .await
            .unwrap();
        let mut b = b.with_gate(Arc::new(CancelOnly));
        let r = b.submit(market("o-2", Side::Buy, dec!(1), 0)).await;
        assert_rejected(r, "market is cancel-only");
        assert!(ExchangeError::Rejected("market is cancel-only").is_order_rejection());
        b.cancel(OrderId::new("o-1").unwrap()).await.unwrap();
        assert!(b.open_orders().is_empty());
    }
}
