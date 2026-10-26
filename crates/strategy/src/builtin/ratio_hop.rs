//! `ratio_hop`: coin accumulation by rotation, after `ccxt/binance-trade-bot`.
//!
//! Hold one coin. When the units of another coin obtainable by selling the
//! current one through the bridge (quote) asset, after both legs' fees,
//! beat the units last held of that coin, rotate `A → bridge → B`. Unit
//! counts therefore only ratchet up per coin. That maximizes units, not
//! value: a coin can be accumulated all the way down. The freefall guard
//! the original lacks refuses to rotate into a coin that fell more than
//! `freefall_pct` over the last `freefall_candles` candles.
//!
//! A live run saves phase, baselines and last evaluation after every
//! event (`save_state`), and a restart restores them, checked against the
//! books: a sell cancelled by the restart's reconcile goes back to holding
//! (or, if part of it sold, buys with what that raised),
//! a cancelled buy is placed again with what's left of its budget, and a
//! sell that
//! filled while the bot was down arrives as a recovered fill and buys with
//! its proceeds. Without saved state (first run, other params, refused
//! state) it starts afresh: holding a sellable coin, it resumes on it (the
//! most valuable if several) with baselines from that evaluation's ratios;
//! holding none, it buys with `quote_amount`.
//!
//! A venue may fill one order in many parts (the paper broker never
//! does), and may report a fill that beat a cancel only after it. So a
//! leg ends only once no order of ours on the coin is open or being
//! cancelled: the buy then spends the whole sale's proceeds, and a stale
//! buy is re-placed for its budget less what already filled. A leg that
//! has started filling is waited out, never cancelled as stale or given
//! up on: cancelling it would leave part of each coin, and the strategy
//! holds one.
//!
//! Decisions use only closed candles and positions read through `Context`
//! (invariants 2 and 5). Expected edge is unverified: rotation pays two
//! fees and the spread each hop, so it only wins when relative moves
//! between the coins mean-revert by more than that.

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, Fee, Fill, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size,
    TimeInForce, Timestamp,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const NAME: &str = "ratio_hop";
/// Evaluations a leg may stay unfilled before the strategy stops, so a
/// refused leg can't wait forever. (Predates `Strategy::on_rejected`,
/// which ratio_hop doesn't use yet.)
const MAX_WAIT: u32 = 2;

/// How each leg is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OrderType {
    /// Taker: fills next open plus slippage; certain but pays the spread.
    Market,
    /// Maker: GTC limit at the last close; fills only if price trades
    /// through it, never worse than the limit, maybe never.
    Limit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RatioHopParams {
    /// Coins to rotate between, all quoted in the same bridge asset. The
    /// first is bought at the start.
    coins: Vec<InstrumentId>,
    /// Quote spent on the initial buy.
    quote_amount: Notional,
    /// Fee per leg assumed when estimating a rotation (`"0.001"`).
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per leg assumed when estimating a rotation, in basis
    /// points (`"5"`). Required: market orders never fill at the close the
    /// decision saw, and a silent 0 would make every hop look cheaper than
    /// it is. Match the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
    /// Extra improvement over the baseline required to rotate (`"0.002"`).
    #[serde(deserialize_with = "exact_decimal", default)]
    scout_margin: Decimal,
    /// Refuse a target that fell more than this fraction (`"0.2"` = 20%)…
    #[serde(deserialize_with = "exact_decimal")]
    freefall_pct: Decimal,
    /// …over this many candles. Engine `history_len` must exceed it.
    freefall_candles: usize,
    /// `"market"` or `"limit"`. Required: the two have different costs and
    /// fill risk, so neither is a safe silent default.
    order_type: OrderType,
    /// Limit mode only: evaluations a leg may rest unfilled before it is
    /// cancelled and, once the cancel is final, re-priced at the close.
    limit_timeout: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Start,
    Buying {
        to: InstrumentId,
        /// Quote this leg may spend: the start amount or the sell proceeds.
        budget: Notional,
        waited: u32,
        /// Quote spent by the fills so far (the venue may split an order).
        #[serde(default)]
        spent: Notional,
        /// The order was cancelled (stale, or by a restart's reconcile);
        /// what's left of the budget is placed again once nothing on the
        /// coin can still fill.
        #[serde(default)]
        cancelling: bool,
    },
    Holding(InstrumentId),
    Selling {
        from: InstrumentId,
        to: InstrumentId,
        waited: u32,
        /// Quote raised by the fills so far, net of quote fees.
        #[serde(default)]
        proceeds: Notional,
        /// The stale sell was cancelled; once nothing on the coin can
        /// still fill, it holds again (or buys with what a late fill
        /// raised).
        #[serde(default)]
        cancelling: bool,
    },
    /// Stopped: bad universe, or a leg never filled.
    Stopped,
}

/// See the module docs.
pub struct RatioHop {
    p: RatioHopParams,
    phase: Phase,
    /// Units of each coin a rotation into it must beat.
    baseline: HashMap<InstrumentId, Qty>,
    last_eval: Option<Timestamp>,
    /// Close time at which `progress` last placed a leg. That close
    /// time's evaluation is skipped: the order isn't in `open_orders` yet,
    /// so with `limit_timeout = 1` it would count as stale at once, be
    /// marked cancelled with nothing cancelled, and never be re-priced.
    placed: Option<Timestamp>,
}

/// What `save_state` writes. Baselines as a sorted list, so the same state
/// always serializes the same way.
#[derive(Serialize, Deserialize)]
struct Saved {
    v: u64,
    phase: Phase,
    baseline: Vec<(InstrumentId, Qty)>,
    last_eval: Option<Timestamp>,
}

impl RatioHop {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: RatioHopParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        let mut uniq = p.coins.clone();
        uniq.sort();
        uniq.dedup();
        if p.coins.len() < 2 || uniq.len() != p.coins.len() {
            return bad("coins: need at least two, no duplicates");
        }
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        if p.scout_margin < Decimal::ZERO {
            return bad("scout_margin must be >= 0");
        }
        if p.freefall_pct <= Decimal::ZERO || p.freefall_pct >= Decimal::ONE {
            return bad("freefall_pct must be in (0, 1)");
        }
        if p.freefall_candles == 0 {
            return bad("freefall_candles must be >= 1");
        }
        if p.order_type == OrderType::Limit && !p.limit_timeout.is_some_and(|t| t >= 1) {
            return bad("limit_timeout (>= 1) is required with order_type = \"limit\"");
        }
        Ok(Box::new(Self {
            p,
            phase: Phase::Start,
            baseline: HashMap::new(),
            last_eval: None,
            placed: None,
        }))
    }

    /// One leg as `order_type` says: a market order, or a GTC limit at the
    /// instrument's last known close. `None` if there's no close yet.
    fn order(
        &self,
        ctx: &Context,
        instrument: &InstrumentId,
        side: Side,
        size: Size,
    ) -> Option<Signal> {
        let kind = match self.p.order_type {
            OrderType::Market => OrderKind::Market,
            OrderType::Limit => OrderKind::Limit {
                price: ctx.last_candle(instrument)?.close,
                tif: TimeInForce::Gtc,
            },
        };
        Some(Signal::Submit {
            instrument: instrument.clone(),
            side,
            size,
            kind,
        })
    }

    /// Cancels for every open order of ours on `id`.
    fn cancels(ctx: &Context, id: &InstrumentId) -> Vec<Signal> {
        ctx.open_orders(id)
            .map(|o| Signal::Cancel(o.id.clone()))
            .collect()
    }

    /// The leg has now rested `waited + 1` evaluations, reaching the limit.
    fn timed_out(&self, waited: u32) -> bool {
        self.p.limit_timeout.is_some_and(|t| waited + 1 >= t)
    }

    /// The coin to resume holding: of the coins held in a sellable amount
    /// (venue filters at the last close), the one worth most at that close.
    /// `None` when flat, as every backtest starts.
    fn held_coin(&self, ctx: &Context) -> Option<InstrumentId> {
        self.p
            .coins
            .iter()
            .filter_map(|id| {
                let inst = ctx.instrument(id)?;
                let units = Self::sellable(ctx, id)?;
                let close = ctx.last_candle(id)?.close;
                inst.check_market(units, close, Side::Sell).ok()?;
                Some((inst.notional(units, close), id))
            })
            .max_by(|x, y| x.0.cmp(&y.0))
            .map(|(_, id)| id.clone())
    }

    /// Lot-rounded units of `id` held, i.e. what a sell can actually move.
    fn sellable(ctx: &Context, id: &InstrumentId) -> Option<Qty> {
        Some(ctx.instrument(id)?.round_qty(ctx.position(id).qty()))
    }

    /// Units of `to` held after selling `units` of `from` and buying `to`
    /// with the proceeds, starting from current closes. Each market leg
    /// pays slippage (sell at close*(1-s) rounded down to tick, buy at
    /// close*(1+s) rounded up, as the paper broker does); a limit leg fills
    /// at its limit, the close, so pays none. Every leg pays `fee`: the
    /// sell fee in quote, the buy fee in base (Binance spot).
    fn estimate(
        &self,
        ctx: &Context,
        from: &InstrumentId,
        units: Qty,
        to: &InstrumentId,
    ) -> Option<Qty> {
        let (fi, ti) = (ctx.instrument(from)?, ctx.instrument(to)?);
        let s = match self.p.order_type {
            OrderType::Market => self.p.slippage_bps / Decimal::from(10_000),
            OrderType::Limit => Decimal::ZERO,
        };
        // round_price(.., Buy) rounds down, (.., Sell) up: against the trader.
        let pf = fi.round_price(ctx.last_candle(from)?.close * (Decimal::ONE - s), Side::Buy);
        let pt = ti.round_price(ctx.last_candle(to)?.close * (Decimal::ONE + s), Side::Sell);
        let keep = Decimal::ONE - self.p.fee;
        let proceeds = fi.notional(units, pf) * keep;
        let bought = ti.round_qty(ti.qty_for(proceeds, pt).ok()?);
        Some(bought * keep)
    }

    /// True if `id` fell more than `freefall_pct` over the last
    /// `freefall_candles` candles, or there isn't that much history yet.
    fn in_freefall(&self, ctx: &Context, id: &InstrumentId) -> bool {
        let n = self.p.freefall_candles;
        let hist: Vec<&Candle> = ctx.history(id).collect();
        if hist.len() <= n {
            return true;
        }
        let then = hist[hist.len() - 1 - n].close;
        let now = hist[hist.len() - 1].close;
        now / then - Decimal::ONE < -self.p.freefall_pct
    }

    fn evaluate(&mut self, ctx: &mut Context) -> Vec<Signal> {
        match self.phase.clone() {
            Phase::Start => {
                if let Some(held) = self.held_coin(ctx) {
                    self.phase = Phase::Holding(held.clone());
                    return self.consider_rotation(ctx, &held);
                }
                let first = self.p.coins[0].clone();
                let budget = self.p.quote_amount;
                let Some(buy) = self.order(ctx, &first, Side::Buy, Size::Quote(budget)) else {
                    return Vec::new();
                };
                self.phase = Phase::Buying {
                    to: first,
                    budget,
                    waited: 0,
                    spent: Notional::ZERO,
                    cancelling: false,
                };
                vec![buy]
            }
            // A leg that has started filling is waited out, never cancelled
            // or given up on (see the module docs); `progress` finishes it,
            // and ends a cancel in flight.
            Phase::Buying {
                cancelling: true, ..
            }
            | Phase::Selling {
                cancelling: true, ..
            } => Vec::new(),
            Phase::Buying {
                to,
                budget,
                waited,
                spent,
                ..
            } => match self.p.order_type {
                // A stale limit buy: cancel; `progress` re-prices it at the
                // close once the cancel is final, so a fill that beat the
                // cancel can't be paid for twice.
                OrderType::Limit if self.timed_out(waited) && spent.is_zero() => {
                    self.phase = Phase::Buying {
                        to: to.clone(),
                        budget,
                        waited: 0,
                        spent,
                        cancelling: true,
                    };
                    Self::cancels(ctx, &to)
                }
                OrderType::Market if waited >= MAX_WAIT && spent.is_zero() => {
                    self.phase = Phase::Stopped;
                    Vec::new()
                }
                _ => {
                    self.phase = Phase::Buying {
                        to,
                        budget,
                        waited: waited + 1,
                        spent,
                        cancelling: false,
                    };
                    Vec::new()
                }
            },
            Phase::Selling {
                from,
                to,
                waited,
                proceeds,
                ..
            } => match self.p.order_type {
                // A stale limit sell: cancel; once final, `progress` goes
                // back to holding and the next evaluation decides afresh.
                OrderType::Limit if self.timed_out(waited) && proceeds.is_zero() => {
                    self.phase = Phase::Selling {
                        from: from.clone(),
                        to,
                        waited: 0,
                        proceeds,
                        cancelling: true,
                    };
                    Self::cancels(ctx, &from)
                }
                OrderType::Market if waited >= MAX_WAIT && proceeds.is_zero() => {
                    self.phase = Phase::Stopped;
                    Vec::new()
                }
                _ => {
                    self.phase = Phase::Selling {
                        from,
                        to,
                        waited: waited + 1,
                        proceeds,
                        cancelling: false,
                    };
                    Vec::new()
                }
            },
            Phase::Holding(a) => self.consider_rotation(ctx, &a),
            Phase::Stopped => Vec::new(),
        }
    }

    fn consider_rotation(&mut self, ctx: &Context, a: &InstrumentId) -> Vec<Signal> {
        let Some(units) = Self::sellable(ctx, a).filter(|u| u.is_positive()) else {
            return Vec::new();
        };
        if self.baseline.is_empty() {
            // First look after the initial buy: today's ratios are the bar
            // every later rotation must beat.
            self.baseline.insert(a.clone(), units);
            for b in self.p.coins.iter().filter(|b| *b != a) {
                if let Some(est) = self.estimate(ctx, a, units, b) {
                    self.baseline.insert(b.clone(), est);
                }
            }
            return Vec::new();
        }
        let need = Decimal::ONE + self.p.scout_margin;
        let best = self
            .p
            .coins
            .iter()
            .filter(|b| *b != a && !self.in_freefall(ctx, b))
            .filter_map(|b| {
                let est = self.estimate(ctx, a, units, b)?;
                let base = *self.baseline.get(b)?;
                (base.is_positive() && est > base * need).then(|| (est / base, b))
            })
            .max_by(|x, y| x.0.cmp(&y.0));
        let Some((_, b)) = best else {
            return Vec::new();
        };
        let b = b.clone();
        self.baseline.insert(a.clone(), units);
        self.phase = Phase::Selling {
            from: a.clone(),
            to: b,
            waited: 0,
            proceeds: Notional::ZERO,
            cancelling: false,
        };
        self.order(ctx, a, Side::Sell, Size::Base(units))
            .into_iter()
            .collect()
    }

    /// Ends a leg, run on every candle after the event's fills (`t` is the
    /// candle's close time). A venue may split one order into many fills,
    /// and one settle may deliver all of them, so `on_fill` only adds them
    /// up; the leg ends once no order of ours on the coin is open or being
    /// cancelled, i.e. no fill can still arrive (the broker drops an order
    /// only once it is closed with every fill delivered). Then:
    /// - sold something: buy the target with the whole proceeds;
    /// - sell cancelled, nothing sold: hold again;
    /// - buy cancelled: place what's left of the budget at the last close;
    /// - bought something: that's the holding.
    fn progress(&mut self, ctx: &Context, t: Timestamp) -> Vec<Signal> {
        let idle = |id: &InstrumentId| {
            ctx.open_orders(id).next().is_none() && ctx.cancelling_orders(id).next().is_none()
        };
        let (to, budget, spent) = match self.phase.clone() {
            Phase::Selling {
                from,
                to,
                proceeds,
                cancelling,
                ..
            } if idle(&from) => {
                if proceeds.is_positive() {
                    // Spend exactly what the sell raised, never more.
                    (to, proceeds, Notional::ZERO)
                } else {
                    if cancelling {
                        self.phase = Phase::Holding(from);
                    }
                    return Vec::new();
                }
            }
            Phase::Buying {
                to,
                budget,
                spent,
                cancelling: true,
                ..
            } if idle(&to) => (to, budget, spent),
            Phase::Buying { to, spent, .. } if spent.is_positive() && idle(&to) => {
                self.phase = Phase::Holding(to);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        let Some(buy) = self.order(ctx, &to, Side::Buy, Size::Quote(budget - spent)) else {
            self.phase = Phase::Stopped;
            return Vec::new();
        };
        self.phase = Phase::Buying {
            to,
            budget,
            waited: 0,
            spent,
            cancelling: false,
        };
        self.placed = Some(t);
        vec![buy]
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: RatioHop::build }
}

impl Strategy for RatioHop {
    fn name(&self) -> &str {
        NAME
    }

    /// Phase, baselines and last evaluation: everything the ratchet has
    /// learned.
    fn save_state(&self) -> Option<serde_json::Value> {
        let mut baseline: Vec<(InstrumentId, Qty)> =
            self.baseline.iter().map(|(k, v)| (k.clone(), *v)).collect();
        baseline.sort_by(|a, b| a.0.cmp(&b.0));
        serde_json::to_value(Saved {
            v: super::STATE_VERSION,
            phase: self.phase.clone(),
            baseline,
            last_eval: self.last_eval,
        })
        .ok()
    }

    /// Checked against the books. A restart's reconcile has cancelled any
    /// order left working, so: `Holding(a)` needs `a` held; `Selling` with
    /// the coin still held means the sell was cancelled, so it becomes
    /// `Holding`, and with the coin gone it waits for the recovered fill;
    /// `Buying` with the target not held places the buy again; `Stopped`
    /// starts over, a restart being the operator stepping in.
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        ctx: &Context,
    ) -> Result<(), StrategyError> {
        if self.phase == Phase::Stopped {
            return Err(super::state_error(
                NAME,
                "universe doesn't fit; staying stopped",
            ));
        }
        super::check_version(NAME, &state)?;
        let saved: Saved =
            serde_json::from_value(state).map_err(|e| super::state_error(NAME, e))?;
        let held = |id: &InstrumentId| Self::sellable(ctx, id).is_some_and(|q| q.is_positive());
        let known = |id: &InstrumentId| self.p.coins.contains(id);
        let phase = match saved.phase {
            Phase::Holding(a) if known(&a) && held(&a) => Phase::Holding(a),
            Phase::Holding(_) => {
                return Err(super::state_error(
                    NAME,
                    "holding a coin the books don't hold",
                ));
            }
            // Partly sold: buy with what it raised (plus any recovered
            // fills) once `progress` sees the leg done.
            Phase::Selling { from, proceeds, .. }
                if known(&from) && held(&from) && proceeds.is_zero() =>
            {
                Phase::Holding(from)
            }
            // The reconcile closed the order, reporting any fill it missed
            // as recovered: `progress` places what's left of the budget.
            Phase::Buying {
                to,
                budget,
                waited,
                spent,
                ..
            } if known(&to) => Phase::Buying {
                to,
                budget,
                waited,
                spent,
                cancelling: true,
            },
            Phase::Buying { .. } => {
                return Err(super::state_error(
                    NAME,
                    "buying a coin not in this universe",
                ));
            }
            Phase::Stopped => Phase::Start,
            other => other,
        };
        if saved.baseline.iter().any(|(_, q)| !q.is_positive()) {
            return Err(super::state_error(NAME, "non-positive baseline"));
        }
        // All checks passed: apply at once.
        self.phase = phase;
        self.baseline = saved.baseline.into_iter().collect();
        self.last_eval = saved.last_eval;
        Ok(())
    }

    fn on_start(&mut self, ctx: &mut Context) {
        let quotes: Option<Vec<_>> = self
            .p
            .coins
            .iter()
            .map(|id| ctx.instrument(id).map(|i| i.quote.clone()))
            .collect();
        let one_bridge = quotes.is_some_and(|q| q.windows(2).all(|w| w[0] == w[1]));
        if !one_bridge {
            // A coin outside the universe or a second quote asset: the
            // ratios would compare different bridges. Refuse to trade.
            self.phase = Phase::Stopped;
        }
    }

    /// Evaluates once per close time, when every coin's candle for it has
    /// arrived, so no coin is judged on a stale price.
    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let t = c.close_time;
        let mut out = self.progress(ctx, t);
        if !self.p.coins.contains(&c.instrument) || self.last_eval == Some(t) {
            return out;
        }
        let complete = self
            .p
            .coins
            .iter()
            .all(|id| ctx.last_candle(id).is_some_and(|l| l.close_time == t));
        if !complete {
            return out;
        }
        self.last_eval = Some(t);
        if self.placed != Some(t) {
            out.extend(self.evaluate(ctx));
        }
        out
    }

    /// Adds the fill to its leg; `progress` acts once the leg is done.
    fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
        let Some(inst) = ctx.instrument(&f.instrument) else {
            return;
        };
        // Only a quote fee comes out of the bridge asset; a base or BNB
        // fee is paid from another balance.
        let fee = match &f.fee {
            Fee::Quote(n) => *n,
            Fee::Base(_) | Fee::Other { .. } => Notional::ZERO,
        };
        let value = inst.notional(f.qty, f.price);
        match &mut self.phase {
            Phase::Selling { from, proceeds, .. }
                if f.instrument == *from && f.side == Side::Sell =>
            {
                *proceeds = *proceeds + value - fee;
            }
            Phase::Buying { to, spent, .. } if f.instrument == *to && f.side == Side::Buy => {
                *spent = *spent + value + fee;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build, names};

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market"
               {extra}"#
        ))
        .unwrap()
    }

    #[test]
    fn limit_mode_builds_with_timeout() {
        assert!(build(NAME, &Params::from_toml(&LIMIT.replace("X", "")).unwrap()).is_ok());
    }

    const LIMIT: &str = r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "limit"
               limit_timeout = 3X"#;

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        for bad in [
            r#"coins = ["binance:BTCUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:BTCUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = 0.001
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "1.5"
               freefall_candles = 2
               order_type = "market""#,
            // slippage_bps missing: must be explicit.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "market""#,
            // order_type missing: must be explicit.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2"#,
            // limit without limit_timeout.
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2
               order_type = "limit""#,
        ] {
            assert!(
                build(NAME, &Params::from_toml(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    mod resume {
        use super::*;
        use crate::driver;
        use bot_core::{
            Asset, AssetClass, CandleSpec, FillSpec, Instrument, InstrumentSpec, Interval,
            Liquidity, Order, OrderId, OrderSpec, Position, Price,
        };
        use rust_decimal_macros::dec;

        fn inst(symbol: &str, base: &str, lot: Decimal) -> Instrument {
            Instrument::new(InstrumentSpec {
                id: format!("binance:{symbol}").parse().unwrap(),
                class: AssetClass::Spot,
                base: Asset::new(base).unwrap(),
                quote: Asset::new("USDT").unwrap(),
                tick_size: Price::new(dec!(0.01)),
                lot_size: Qty::new(lot),
                min_qty: Qty::new(lot),
                min_notional: Notional::new(dec!(5)),
            })
            .unwrap()
        }

        fn btc() -> Instrument {
            inst("BTCUSDT", "BTC", dec!(0.00001))
        }

        fn eth() -> Instrument {
            inst("ETHUSDT", "ETH", dec!(0.0001))
        }

        fn held(i: &Instrument, qty: Decimal, cost: Decimal) -> Position {
            Position::from_parts(
                i.id.clone(),
                Qty::new(qty),
                Notional::new(cost),
                Notional::ZERO,
            )
            .unwrap()
        }

        const HOUR: i64 = 3_600_000;

        /// One hourly close of both coins, fed the way the engine does;
        /// returns every signal the strategy emitted.
        fn close(
            s: &mut dyn Strategy,
            ctx: &mut Context,
            n: i64,
            btc_px: Decimal,
            eth_px: Decimal,
        ) -> Vec<Signal> {
            let mut out = Vec::new();
            for (i, px) in [(btc(), btc_px), (eth(), eth_px)] {
                let c = Candle::new(CandleSpec {
                    instrument: i.id.clone(),
                    interval: Interval::H1,
                    open_time: Timestamp::from_millis(n * HOUR),
                    close_time: Timestamp::from_millis((n + 1) * HOUR - 1),
                    open: Price::new(px),
                    high: Price::new(px),
                    low: Price::new(px),
                    close: Price::new(px),
                    volume: Qty::new(dec!(1)),
                    quote_volume: Notional::new(px),
                })
                .unwrap();
                driver::set_now(ctx, c.close_time);
                driver::push_candle(ctx, c.clone());
                out.extend(s.on_candle(ctx, &c));
                out.extend(driver::take_signals(ctx));
            }
            out
        }

        fn start(positions: Vec<Position>) -> (Box<dyn Strategy>, Context) {
            let p = params("");
            let mut ctx = driver::new(vec![btc(), eth()], p.clone(), 10);
            for pos in positions {
                driver::set_position(&mut ctx, pos);
            }
            let mut s = build(NAME, &p).unwrap();
            s.on_start(&mut ctx);
            (s, ctx)
        }

        fn sell_btc(qty: Decimal) -> Signal {
            Signal::Submit {
                instrument: btc().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(qty)),
                kind: OrderKind::Market,
            }
        }

        /// Hand-worked, fee 0.001, slippage 10 bps, BTC flat at 60000:
        /// selling 0.01 BTC fills at 59940 -> 599.4 * 0.999 = 598.8006 USDT.
        /// t0, ETH 3000 (buy at 3003): lot(0.199400...) = 0.1994 * 0.999 =
        /// 0.1992006 ETH, the baseline. t2, ETH 2900 (buy at 2902.90):
        /// lot(0.206276...) = 0.2062 * 0.999 = 0.2059938 ETH > 0.1992006,
        /// and 2900/3000 is no freefall at 20%: rotate, selling 0.01 BTC.
        #[test]
        fn restart_holding_a_coin_resumes_instead_of_buying() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            assert!(close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)).is_empty());
            assert!(close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000)).is_empty());
            assert_eq!(
                close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900)),
                vec![sell_btc(dec!(0.01))]
            );
        }

        #[test]
        fn flat_start_still_buys_the_first_coin() {
            let (mut s, mut ctx) = start(vec![]);
            assert_eq!(
                close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)),
                vec![Signal::Submit {
                    instrument: btc().id.clone(),
                    side: Side::Buy,
                    size: Size::Quote(Notional::new(dec!(1000))),
                    kind: OrderKind::Market,
                }]
            );
        }

        /// 0.01 BTC (600 USDT) outweighs 0.1 ETH (300 USDT): BTC is the
        /// held coin, so the t2 rotation sells BTC.
        #[test]
        fn several_coins_held_resumes_the_most_valuable() {
            let (mut s, mut ctx) = start(vec![
                held(&btc(), dec!(0.01), dec!(600)),
                held(&eth(), dec!(0.1), dec!(300)),
            ]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900)),
                vec![sell_btc(dec!(0.01))]
            );
        }

        /// 0.00001 BTC is worth 0.6 USDT, under the 5 USDT minimum: it
        /// can't be sold, so it isn't a holding to resume.
        #[test]
        fn unsellable_dust_is_not_a_holding() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.00001), dec!(0.6))]);
            let first = close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            assert!(matches!(
                first.as_slice(),
                [Signal::Submit {
                    side: Side::Buy,
                    ..
                }]
            ));
        }

        /// After warming up on a held coin (baselines set), the saved state
        /// restores into a fresh strategy exactly.
        #[test]
        fn state_round_trips() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            let state = s.save_state().unwrap();
            assert_eq!(
                state["phase"],
                serde_json::json!({ "holding": "binance:BTCUSDT" })
            );
            assert_eq!(state["baseline"].as_array().unwrap().len(), 2);
            let mut fresh = build(NAME, &params("")).unwrap();
            fresh.on_start(&mut ctx);
            fresh.restore_state(state.clone(), &ctx).unwrap();
            assert_eq!(fresh.save_state(), Some(state));
        }

        /// Saved as holding BTC, but the books are flat: refused, and the
        /// strategy is left as it was.
        #[test]
        fn holding_what_the_books_dont_hold_is_refused() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            let state = s.save_state().unwrap();
            let (mut flat, ctx) = start(vec![]);
            let before = flat.save_state();
            assert!(flat.restore_state(state, &ctx).is_err());
            assert_eq!(flat.save_state(), before);
        }

        fn saved(phase: serde_json::Value) -> serde_json::Value {
            serde_json::json!({ "v": 1, "phase": phase, "baseline": [], "last_eval": null })
        }

        /// Saved mid-sell, but BTC is still held: the restart's reconcile
        /// cancelled the sell, so it is holding BTC again.
        #[test]
        fn cancelled_sell_restores_as_holding() {
            let (mut s, ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            let state = saved(serde_json::json!({ "selling": {
                "from": "binance:BTCUSDT", "to": "binance:ETHUSDT", "waited": 0 } }));
            s.restore_state(state, &ctx).unwrap();
            assert_eq!(
                s.save_state().unwrap()["phase"],
                serde_json::json!({ "holding": "binance:BTCUSDT" })
            );
        }

        /// Saved mid-buy with 500 USDT to spend, ETH not held: the buy is
        /// placed again, same budget, at the next evaluation.
        #[test]
        fn cancelled_buy_is_placed_again() {
            let (mut s, mut ctx) = start(vec![]);
            let state = saved(serde_json::json!({ "buying": {
                "to": "binance:ETHUSDT", "budget": "500", "waited": 1 } }));
            s.restore_state(state, &ctx).unwrap();
            assert_eq!(
                close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)),
                vec![Signal::Submit {
                    instrument: eth().id.clone(),
                    side: Side::Buy,
                    size: Size::Quote(Notional::new(dec!(500))),
                    kind: OrderKind::Market,
                }]
            );
            // Placed once, not on every evaluation.
            assert!(close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000)).is_empty());
        }

        fn start_with(p: Params, positions: Vec<Position>) -> (Box<dyn Strategy>, Context) {
            let mut ctx = driver::new(vec![btc(), eth()], p.clone(), 10);
            for pos in positions {
                driver::set_position(&mut ctx, pos);
            }
            let mut s = build(NAME, &p).unwrap();
            s.on_start(&mut ctx);
            (s, ctx)
        }

        /// The sell of 0.01 BTC as the venue sees it, still working.
        fn working_sell(kind: OrderKind) -> Order {
            Order::new(OrderSpec {
                id: OrderId::new("o-1").unwrap(),
                instrument: btc().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(0.01))),
                kind,
                created_at: Timestamp::from_millis(3 * HOUR - 1),
            })
            .unwrap()
        }

        /// One part of an order, the fee 0.1% of its notional: in quote
        /// for a sell, in base for a buy (Binance spot).
        fn part(i: &Instrument, side: Side, qty: Decimal, px: Decimal, liq: Liquidity) -> Fill {
            let fee = match side {
                Side::Sell => Fee::Quote(Notional::new(qty * px * dec!(0.001))),
                Side::Buy => Fee::Base(Qty::new(qty * dec!(0.001))),
            };
            Fill::new(FillSpec {
                order_id: OrderId::new("o-1").unwrap(),
                instrument: i.id.clone(),
                side,
                qty: Qty::new(qty),
                price: Price::new(px),
                fee,
                liquidity: liq,
                ts: Timestamp::from_millis(3 * HOUR),
            })
            .unwrap()
        }

        /// Delivers one settle's fills the way the engine does: open orders
        /// as of the settle, then each fill with the position after it.
        fn settle(
            s: &mut dyn Strategy,
            ctx: &mut Context,
            open: Vec<Order>,
            fills: &[Fill],
            btc_left: Decimal,
        ) {
            driver::set_open_orders(ctx, open);
            for f in fills {
                driver::set_position(ctx, held(&btc(), btc_left, btc_left * dec!(60000)));
                s.on_fill(ctx, f);
            }
        }

        fn buy_eth(budget: Decimal, kind: OrderKind) -> Signal {
            Signal::Submit {
                instrument: eth().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(budget)),
                kind,
            }
        }

        /// The t2 rotation's sell of 0.01 BTC at 59940 fills as 0.004 then
        /// 0.006, both reported in one settle. Proceeds: 239.76 - 0.23976 =
        /// 239.52024, plus 359.64 - 0.35964 = 359.28036, total 598.8006
        /// (= the unsplit sale). The buy spends all of it, not the first
        /// part's 239.52024.
        #[test]
        fn a_split_sell_buys_with_the_whole_proceeds() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900));
            let fills = [
                part(
                    &btc(),
                    Side::Sell,
                    dec!(0.004),
                    dec!(59940),
                    Liquidity::Taker,
                ),
                part(
                    &btc(),
                    Side::Sell,
                    dec!(0.006),
                    dec!(59940),
                    Liquidity::Taker,
                ),
            ];
            settle(s.as_mut(), &mut ctx, vec![], &fills, dec!(0));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 3, dec!(60000), dec!(2900)),
                vec![buy_eth(dec!(598.8006), OrderKind::Market)]
            );
        }

        /// Same split, but the parts arrive in different settles and the
        /// order stays working for three evaluations, past `MAX_WAIT`: a
        /// leg that has started filling is waited out, not stopped, and the
        /// buy comes only with the last part.
        #[test]
        fn a_sell_still_working_waits_for_its_last_fill() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900));
            let working = vec![working_sell(OrderKind::Market)];
            let first = part(
                &btc(),
                Side::Sell,
                dec!(0.004),
                dec!(59940),
                Liquidity::Taker,
            );
            settle(s.as_mut(), &mut ctx, working.clone(), &[first], dec!(0.006));
            for n in 3..6 {
                assert!(close(s.as_mut(), &mut ctx, n, dec!(60000), dec!(2900)).is_empty());
                driver::set_open_orders(&mut ctx, working.clone());
            }
            let last = part(
                &btc(),
                Side::Sell,
                dec!(0.006),
                dec!(59940),
                Liquidity::Taker,
            );
            settle(s.as_mut(), &mut ctx, vec![], &[last], dec!(0));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 6, dec!(60000), dec!(2900)),
                vec![buy_eth(dec!(598.8006), OrderKind::Market)]
            );
        }

        /// Limit mode, timeout 3. Estimates without slippage: baseline
        /// lot(599.4 / 3000) * 0.999 = 0.1996002 ETH; at 2900,
        /// lot(599.4 / 2900) * 0.999 = 0.2063934 beats it, so t2 sells
        /// 0.01 BTC at 60000. Unfilled, the sell is cancelled at t5 (its
        /// third evaluation). Partly filled (0.004 at 60000: 240 - 0.24 =
        /// 239.76), it is left working instead; the 0.006 rest (360 - 0.36
        /// = 359.64) buys ETH at the 2900 close with 599.4.
        #[test]
        fn a_stale_limit_sell_is_cancelled_only_if_nothing_filled() {
            let limit = Params::from_toml(&LIMIT.replace("X", "")).unwrap();
            let sell_kind = OrderKind::Limit {
                price: Price::new(dec!(60000)),
                tif: TimeInForce::Gtc,
            };
            let working = vec![working_sell(sell_kind)];
            let rotate = |s: &mut dyn Strategy, ctx: &mut Context| {
                close(s, ctx, 0, dec!(60000), dec!(3000));
                close(s, ctx, 1, dec!(60000), dec!(3000));
                close(s, ctx, 2, dec!(60000), dec!(2900))
            };

            let (mut s, mut ctx) =
                start_with(limit.clone(), vec![held(&btc(), dec!(0.01), dec!(600))]);
            assert_eq!(rotate(s.as_mut(), &mut ctx).len(), 1);
            driver::set_open_orders(&mut ctx, working.clone());
            assert!(close(s.as_mut(), &mut ctx, 3, dec!(60000), dec!(2900)).is_empty());
            assert!(close(s.as_mut(), &mut ctx, 4, dec!(60000), dec!(2900)).is_empty());
            assert_eq!(
                close(s.as_mut(), &mut ctx, 5, dec!(60000), dec!(2900)),
                vec![Signal::Cancel(OrderId::new("o-1").unwrap())]
            );

            let (mut s, mut ctx) = start_with(limit, vec![held(&btc(), dec!(0.01), dec!(600))]);
            rotate(s.as_mut(), &mut ctx);
            let first = part(
                &btc(),
                Side::Sell,
                dec!(0.004),
                dec!(60000),
                Liquidity::Maker,
            );
            settle(s.as_mut(), &mut ctx, working.clone(), &[first], dec!(0.006));
            for n in 3..6 {
                assert!(close(s.as_mut(), &mut ctx, n, dec!(60000), dec!(2900)).is_empty());
            }
            let last = part(
                &btc(),
                Side::Sell,
                dec!(0.006),
                dec!(60000),
                Liquidity::Maker,
            );
            settle(s.as_mut(), &mut ctx, vec![], &[last], dec!(0));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 6, dec!(60000), dec!(2900)),
                vec![buy_eth(
                    dec!(599.4),
                    OrderKind::Limit {
                        price: Price::new(dec!(2900)),
                        tif: TimeInForce::Gtc,
                    }
                )]
            );
        }

        /// A buy filling in parts stays a buy until its order is done:
        /// 0.1 ETH at 3003 (300.3 spent; 0.0999 held after the base fee)
        /// with the order still working, then the rest; only then is ETH
        /// the holding.
        #[test]
        fn a_split_buy_holds_only_once_done() {
            let (mut s, mut ctx) = start(vec![held(&eth(), dec!(0.0999), dec!(300.3))]);
            let state = saved(serde_json::json!({ "buying": {
                "to": "binance:ETHUSDT", "budget": "598.8006", "waited": 0, "spent": "0" } }));
            s.restore_state(state, &ctx).unwrap();
            let buy = Order::new(OrderSpec {
                id: OrderId::new("o-1").unwrap(),
                instrument: eth().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(598.8006))),
                kind: OrderKind::Market,
                created_at: Timestamp::from_millis(3 * HOUR - 1),
            })
            .unwrap();
            driver::set_open_orders(&mut ctx, vec![buy]);
            s.on_fill(
                &mut ctx,
                &part(&eth(), Side::Buy, dec!(0.1), dec!(3003), Liquidity::Taker),
            );
            close(s.as_mut(), &mut ctx, 3, dec!(60000), dec!(3000));
            let phase = &s.save_state().unwrap()["phase"];
            assert_eq!(phase["buying"]["spent"], "300.3");

            driver::set_open_orders(&mut ctx, vec![]);
            s.on_fill(
                &mut ctx,
                &part(
                    &eth(),
                    Side::Buy,
                    dec!(0.0994),
                    dec!(3003),
                    Liquidity::Taker,
                ),
            );
            close(s.as_mut(), &mut ctx, 4, dec!(60000), dec!(3000));
            assert_eq!(
                s.save_state().unwrap()["phase"],
                serde_json::json!({ "holding": "binance:ETHUSDT" })
            );
        }

        /// Saved mid-sell with 239.52024 already raised and 0.006 BTC left:
        /// the restart's reconcile cancelled the rest, so the strategy buys
        /// with what was raised rather than forgetting it as holding BTC.
        #[test]
        fn a_partly_filled_sell_restores_and_buys_with_its_proceeds() {
            let (mut s, mut ctx) = start(vec![held(&btc(), dec!(0.006), dec!(360))]);
            let state = saved(serde_json::json!({ "selling": {
                "from": "binance:BTCUSDT", "to": "binance:ETHUSDT", "waited": 1,
                "proceeds": "239.52024" } }));
            s.restore_state(state, &ctx).unwrap();
            assert_eq!(
                close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000)),
                vec![buy_eth(dec!(239.52024), OrderKind::Market)]
            );
        }

        fn limit_params(timeout: u32) -> Params {
            Params::from_toml(&LIMIT.replace("3X", &timeout.to_string())).unwrap()
        }

        fn limit_at(px: Decimal) -> OrderKind {
            OrderKind::Limit {
                price: Price::new(px),
                tif: TimeInForce::Gtc,
            }
        }

        /// Limit mode, timeout 3: the t2 sell of 0.01 BTC at 60000 rests
        /// unfilled and is cancelled at t5. A fill of 0.004 beat the cancel
        /// and is reported at t6, while the venue hasn't confirmed the
        /// cancel: nothing yet. At t7 the cancel is final: buy ETH at the
        /// 2900 close with the late fill's 240 - 0.24 = 239.76, not drop it.
        #[test]
        fn a_fill_that_beats_a_sell_cancel_is_bought_with() {
            let (mut s, mut ctx) =
                start_with(limit_params(3), vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900));
            let sell = working_sell(limit_at(dec!(60000)));
            driver::set_open_orders(&mut ctx, vec![sell.clone()]);
            for n in 3..5 {
                assert!(close(s.as_mut(), &mut ctx, n, dec!(60000), dec!(2900)).is_empty());
            }
            assert_eq!(
                close(s.as_mut(), &mut ctx, 5, dec!(60000), dec!(2900)),
                vec![Signal::Cancel(sell.id.clone())]
            );
            driver::set_cancelling_orders(&mut ctx, vec![sell]);
            let late = part(
                &btc(),
                Side::Sell,
                dec!(0.004),
                dec!(60000),
                Liquidity::Maker,
            );
            settle(s.as_mut(), &mut ctx, vec![], &[late], dec!(0.006));
            assert!(close(s.as_mut(), &mut ctx, 6, dec!(60000), dec!(2900)).is_empty());
            driver::set_cancelling_orders(&mut ctx, vec![]);
            assert_eq!(
                close(s.as_mut(), &mut ctx, 7, dec!(60000), dec!(2900)),
                vec![buy_eth(dec!(239.76), limit_at(dec!(2900)))]
            );
        }

        /// Limit mode, timeout 3, flat start: t0 places a 1000 USDT limit
        /// buy of BTC at 60000, cancelled as stale at t3. 0.005 BTC
        /// (300 USDT) filled before the cancel landed. Once the cancel is
        /// final the buy is placed again for the 700 left, not 1000.
        #[test]
        fn a_fill_that_beats_a_buy_cancel_is_not_paid_twice() {
            let (mut s, mut ctx) = start_with(limit_params(3), vec![]);
            let first = close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            let Some(Signal::Submit { kind, .. }) = first.first() else {
                panic!("{first:?}");
            };
            // As risk sizes it: lot(1000 / 60000) = 0.01666 BTC.
            let buy = Order::new(OrderSpec {
                id: OrderId::new("o-1").unwrap(),
                instrument: btc().id.clone(),
                side: Side::Buy,
                size: Size::Base(Qty::new(dec!(0.01666))),
                kind: *kind,
                created_at: Timestamp::from_millis(HOUR - 1),
            })
            .unwrap();
            driver::set_open_orders(&mut ctx, vec![buy.clone()]);
            assert!(close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000)).is_empty());
            assert!(close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(3000)).is_empty());
            assert_eq!(
                close(s.as_mut(), &mut ctx, 3, dec!(60000), dec!(3000)),
                vec![Signal::Cancel(buy.id.clone())]
            );
            driver::set_open_orders(&mut ctx, vec![]);
            driver::set_cancelling_orders(&mut ctx, vec![buy]);
            let late = part(
                &btc(),
                Side::Buy,
                dec!(0.005),
                dec!(60000),
                Liquidity::Maker,
            );
            s.on_fill(&mut ctx, &late);
            assert!(close(s.as_mut(), &mut ctx, 4, dec!(60000), dec!(3000)).is_empty());
            driver::set_cancelling_orders(&mut ctx, vec![]);
            assert_eq!(
                close(s.as_mut(), &mut ctx, 5, dec!(60000), dec!(3000)),
                vec![Signal::Submit {
                    instrument: btc().id.clone(),
                    side: Side::Buy,
                    size: Size::Quote(Notional::new(dec!(700))),
                    kind: limit_at(dec!(60000)),
                }]
            );
        }

        /// Limit mode, timeout 1: the t2 sell (0.01 BTC at 60000) fills
        /// whole at t3 for 600 - 0.6 = 599.4, and the ETH buy is placed
        /// then. It isn't in `open_orders` until the next settle, so the
        /// same close time's evaluation must not count it as stale: it
        /// would mark it cancelled without cancelling anything, and it
        /// would never be re-priced. Resting unfilled through t4 (as
        /// lot(599.4 / 2900) = 0.2066 ETH), it is cancelled then.
        #[test]
        fn a_leg_placed_mid_close_rests_a_full_evaluation() {
            let (mut s, mut ctx) =
                start_with(limit_params(1), vec![held(&btc(), dec!(0.01), dec!(600))]);
            close(s.as_mut(), &mut ctx, 0, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 1, dec!(60000), dec!(3000));
            close(s.as_mut(), &mut ctx, 2, dec!(60000), dec!(2900));
            let sold = part(
                &btc(),
                Side::Sell,
                dec!(0.01),
                dec!(60000),
                Liquidity::Maker,
            );
            settle(s.as_mut(), &mut ctx, vec![], &[sold], dec!(0));
            assert_eq!(
                close(s.as_mut(), &mut ctx, 3, dec!(60000), dec!(2900)),
                vec![buy_eth(dec!(599.4), limit_at(dec!(2900)))]
            );
            let buy = Order::new(OrderSpec {
                id: OrderId::new("o-2").unwrap(),
                instrument: eth().id.clone(),
                side: Side::Buy,
                size: Size::Base(Qty::new(dec!(0.2066))),
                kind: limit_at(dec!(2900)),
                created_at: Timestamp::from_millis(4 * HOUR - 1),
            })
            .unwrap();
            driver::set_open_orders(&mut ctx, vec![buy.clone()]);
            assert_eq!(
                close(s.as_mut(), &mut ctx, 4, dec!(60000), dec!(2900)),
                vec![Signal::Cancel(buy.id.clone())]
            );
        }

        #[test]
        fn other_versions_are_refused() {
            let (mut s, ctx) = start(vec![]);
            let mut state = saved(serde_json::json!("start"));
            state["v"] = serde_json::json!(2);
            assert!(s.restore_state(state, &ctx).is_err());
        }
    }
}
