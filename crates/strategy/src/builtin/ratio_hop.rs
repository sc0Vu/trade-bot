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
//! Decisions use only closed candles and positions read through `Context`
//! (invariants 2 and 5). Expected edge is unverified: rotation pays two
//! fees and the spread each hop, so it only wins when relative moves
//! between the coins mean-revert by more than that.

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, Fee, Fill, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size,
    Timestamp,
};
use serde::Deserialize;
use std::collections::HashMap;

const NAME: &str = "ratio_hop";
/// Evaluations a leg may stay unfilled before the strategy stops. Strategies
/// can't see rejections yet, so a refused leg would otherwise wait forever.
const MAX_WAIT: u32 = 2;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    Buying {
        to: InstrumentId,
        waited: u32,
    },
    Holding(InstrumentId),
    Selling {
        from: InstrumentId,
        to: InstrumentId,
        waited: u32,
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
        Ok(Box::new(Self {
            p,
            phase: Phase::Start,
            baseline: HashMap::new(),
            last_eval: None,
        }))
    }

    fn market(instrument: &InstrumentId, side: Side, size: Size) -> Signal {
        Signal::Submit {
            instrument: instrument.clone(),
            side,
            size,
            kind: OrderKind::Market,
        }
    }

    /// Lot-rounded units of `id` held, i.e. what a sell can actually move.
    fn sellable(ctx: &Context, id: &InstrumentId) -> Option<Qty> {
        Some(ctx.instrument(id)?.round_qty(ctx.position(id).qty()))
    }

    /// Units of `to` held after selling `units` of `from` and buying `to`
    /// with the proceeds, starting from current closes. Each leg pays
    /// slippage (sell at close*(1-s) rounded down to tick, buy at
    /// close*(1+s) rounded up, as the paper broker does) and `fee`: the
    /// sell fee in quote, the buy fee in base (Binance spot).
    fn estimate(
        &self,
        ctx: &Context,
        from: &InstrumentId,
        units: Qty,
        to: &InstrumentId,
    ) -> Option<Qty> {
        let (fi, ti) = (ctx.instrument(from)?, ctx.instrument(to)?);
        let s = self.p.slippage_bps / Decimal::from(10_000);
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
                let first = &self.p.coins[0];
                self.phase = Phase::Buying {
                    to: first.clone(),
                    waited: 0,
                };
                vec![Self::market(
                    first,
                    Side::Buy,
                    Size::Quote(self.p.quote_amount),
                )]
            }
            Phase::Buying { to, waited } => {
                self.phase = if waited >= MAX_WAIT {
                    Phase::Stopped
                } else {
                    Phase::Buying {
                        to,
                        waited: waited + 1,
                    }
                };
                Vec::new()
            }
            Phase::Selling { from, to, waited } => {
                self.phase = if waited >= MAX_WAIT {
                    Phase::Stopped
                } else {
                    Phase::Selling {
                        from,
                        to,
                        waited: waited + 1,
                    }
                };
                Vec::new()
            }
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
        };
        vec![Self::market(a, Side::Sell, Size::Base(units))]
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: RatioHop::build }
}

impl Strategy for RatioHop {
    fn name(&self) -> &str {
        NAME
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
        if !self.p.coins.contains(&c.instrument) || self.last_eval == Some(t) {
            return Vec::new();
        }
        let complete = self
            .p
            .coins
            .iter()
            .all(|id| ctx.last_candle(id).is_some_and(|l| l.close_time == t));
        if !complete {
            return Vec::new();
        }
        self.last_eval = Some(t);
        self.evaluate(ctx)
    }

    fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
        match self.phase.clone() {
            Phase::Selling { from, to, .. } if f.instrument == from && f.side == Side::Sell => {
                let Some(inst) = ctx.instrument(&from) else {
                    self.phase = Phase::Stopped;
                    return;
                };
                let fee = match &f.fee {
                    Fee::Quote(n) => *n,
                    Fee::Base(_) | Fee::Other { .. } => Notional::ZERO,
                };
                let proceeds = inst.notional(f.qty, f.price) - fee;
                // Spend exactly what the sell raised, never more.
                ctx.submit(Self::market(&to, Side::Buy, Size::Quote(proceeds)));
                self.phase = Phase::Buying { to, waited: 0 };
            }
            Phase::Buying { to, .. } if f.instrument == to && f.side == Side::Buy => {
                self.phase = Phase::Holding(to);
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
               {extra}"#
        ))
        .unwrap()
    }

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
               freefall_candles = 2"#,
            r#"coins = ["binance:BTCUSDT", "binance:BTCUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2"#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = 0.001
               slippage_bps = "10"
               freefall_pct = "0.2"
               freefall_candles = 2"#,
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               freefall_pct = "1.5"
               freefall_candles = 2"#,
        ] {
            assert!(
                build(NAME, &Params::from_toml(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }
}
