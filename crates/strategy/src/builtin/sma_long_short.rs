//! `sma_long_short`: `sma_trend` that also shorts where it can.
//!
//! One coin, a fixed position size `notional` (quote) re-derived at each
//! entry: long while the close is above the simple moving average of the
//! last `sma_period` closes by more than `band_pct`, short while below it
//! by more than `band_pct`, unchanged in the dead zone. A flip closes the
//! position reduce-only and opens the other side at the next decision:
//! one order for both needs margin for both, and after a loss the broker
//! refuses it, leaving the losing position open bar after bar. A
//! reduce-only close needs no margin.
//!
//! Short only on an instrument that allows it ([`Instrument::allows_short`]:
//! perpetuals). On spot the short side is "flat" — the same rule as
//! `sma_trend`, sized in base units instead of a quote budget.
//!
//! Size is `notional / close`, lot-rounded, at the deciding close; leverage
//! and margin are the broker's (`--leverage` in a backtest): the strategy
//! never sees them (invariant 5). Dust (a position the venue wouldn't take
//! an order for) counts as flat. No decision while an order on the coin is
//! working or cancelling. Stateless.
//!
//! Edge unverified: a symmetric trend rule pays a round trip per whipsaw
//! (two taker fees, two slippages) and, short or long, funding every
//! settlement (positive rates make longs pay; shorts receive them, so in a
//! long-biased market a short's carry is a small tailwind, not an edge).
//! Leverage scales both the trend's payoff and the whipsaw's cost, and puts
//! a liquidation price under every position.
//!
//! [`Instrument::allows_short`]: bot_core::Instrument::allows_short

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "sma_long_short";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongShortParams {
    /// The one instrument traded (`"binance-usdm:BTCUSDT"`).
    coin: InstrumentId,
    /// Closes in the average, current bar included; ≤ `history_len`.
    sma_period: usize,
    /// Position size in quote at entry (`"1000"`): notional, not margin.
    notional: Notional,
    /// Dead zone around the average (`"0.01"` = 1%); default none.
    #[serde(deserialize_with = "exact_decimal", default)]
    band_pct: Decimal,
    /// Fee per trade assumed; required, range-checked, unused in decisions.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed (bps); required, unused.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// Which way the books hold the coin, dust counted as flat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Long(Qty),
    Short(Qty),
    Flat,
}

/// See the module docs.
pub struct SmaLongShort {
    p: LongShortParams,
}

impl SmaLongShort {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: LongShortParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.sma_period == 0 {
            return bad("sma_period must be >= 1");
        }
        if !p.notional.is_positive() {
            return bad("notional must be positive");
        }
        if p.band_pct < Decimal::ZERO || p.band_pct >= Decimal::ONE {
            return bad("band_pct must be in [0, 1)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self { p }))
    }

    fn sma(&self, ctx: &Context) -> Option<Decimal> {
        let n = self.p.sma_period;
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n {
            return None;
        }
        let sum: Decimal = hist.rev().take(n).map(|c| c.close.get()).sum();
        Some(sum / Decimal::from(n))
    }

    fn held(&self, ctx: &Context, c: &Candle) -> Held {
        let Some(inst) = ctx.instrument(&self.p.coin) else {
            return Held::Flat;
        };
        let qty = ctx.position(&self.p.coin).qty();
        let units = inst.round_qty(qty.abs());
        let (held, closing) = if qty.is_negative() {
            (Held::Short(units), Side::Buy)
        } else {
            (Held::Long(units), Side::Sell)
        };
        if inst.check_market(units, c.close, closing).is_ok() {
            held
        } else {
            Held::Flat
        }
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: SmaLongShort::build }
}

impl Strategy for SmaLongShort {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from history and positions alone: only the format
    /// version is saved.
    fn save_state(&self) -> Option<serde_json::Value> {
        super::stateless_save()
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::stateless_restore(NAME, &state)
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        if c.instrument != *coin
            || ctx.open_orders(coin).next().is_some()
            || ctx.cancelling_orders(coin).next().is_some()
        {
            return Vec::new();
        }
        let (Some(sma), Some(inst)) = (self.sma(ctx), ctx.instrument(coin)) else {
            return Vec::new();
        };
        let close = c.close.get();
        let band = self.p.band_pct;
        let up = close > sma * (Decimal::ONE + band);
        let down = close < sma * (Decimal::ONE - band);
        let fresh = || {
            inst.qty_for(self.p.notional, c.close)
                .ok()
                .map(|q| inst.round_qty(q))
                .unwrap_or(Qty::ZERO)
        };
        // Closing (a flip's first half, or long → flat on spot) is
        // reduce-only; opening comes from flat.
        let (side, units, reduce) = match (self.held(ctx, c), up, down) {
            (Held::Flat, true, _) => (Side::Buy, fresh(), false),
            (Held::Short(s), true, _) => (Side::Buy, s, true),
            (Held::Long(l), _, true) => (Side::Sell, l, true),
            (Held::Flat, _, true) if inst.allows_short() => (Side::Sell, fresh(), false),
            _ => return Vec::new(),
        };
        if inst.check_market(units, c.close, side).is_err() {
            return Vec::new();
        }
        vec![if reduce {
            Signal::Reduce {
                instrument: coin.clone(),
                side,
                qty: units,
                kind: OrderKind::Market,
            }
        } else {
            Signal::Submit {
                instrument: coin.clone(),
                side,
                size: Size::Base(units),
                kind: OrderKind::Market,
            }
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{AssetClass, Instrument, InstrumentSpec, Position};
    use rust_decimal_macros::dec;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               sma_period = 3
               notional = "1000"
               fee = "0.0005"
               slippage_bps = "2"
               {extra}"#
        ))
        .unwrap()
    }

    /// The test BTCUSDT (lot 0.00001), as a perpetual or spot.
    fn inst(perp: bool) -> Instrument {
        let class = if perp {
            AssetClass::Perpetual
        } else {
            AssetClass::Spot
        };
        Instrument::new(InstrumentSpec {
            class,
            ..(*btcusdt()).clone()
        })
        .unwrap()
    }

    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        s.on_candle(ctx, &c)
    }

    fn hold(ctx: &mut Context, qty: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(qty),
                Notional::new(qty * dec!(100)),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn order(side: Side, qty: Decimal) -> Vec<Signal> {
        vec![Signal::Submit {
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
        }]
    }

    fn close(side: Side, qty: Decimal) -> Vec<Signal> {
        vec![Signal::Reduce {
            instrument: btcusdt().id.clone(),
            side,
            qty: Qty::new(qty),
            kind: OrderKind::Market,
        }]
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
        for extra in ["band_pct = \"1\"", "band_pct = 0.01"] {
            assert!(build(NAME, &params(extra)).is_err(), "{extra}");
        }
    }

    /// SMA(3). 100, 100, 100: on the average, nothing. 110: SMA 103.33 →
    /// long 1000/110 = 9.09090 (lot). Held: 90 → SMA 100 → flip, first
    /// half: close the 9.09090, reduce-only. Flat: 80 → SMA 93.33, below →
    /// short 1000/80 = 12.5. 120: SMA (90+80+120)/3 = 96.67 → flip: close
    /// the 12.5 short, reduce-only.
    #[test]
    fn flips_between_long_and_short_on_a_perpetual() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = driver::new(vec![inst(true)], params(""), 10);
        for i in 0..3 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty());
        }
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 3, "110"),
            order(Side::Buy, dec!(9.0909))
        );
        hold(&mut ctx, dec!(9.0909));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 4, "90"),
            close(Side::Sell, dec!(9.0909))
        );
        hold(&mut ctx, dec!(0));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 5, "80"),
            order(Side::Sell, dec!(12.5))
        );
        hold(&mut ctx, dec!(-12.5));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 6, "120"),
            close(Side::Buy, dec!(12.5))
        );
    }

    /// Same closes on spot: the long is opened as on the perpetual, but the
    /// fall only sells what is held (no short), and the next fall from
    /// flat does nothing.
    #[test]
    fn on_spot_the_short_side_is_flat() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = driver::new(vec![inst(false)], params(""), 10);
        for i in 0..3 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 3, "110"),
            order(Side::Buy, dec!(9.0909))
        );
        hold(&mut ctx, dec!(9.0909));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 4, "90"),
            close(Side::Sell, dec!(9.0909))
        );
        hold(&mut ctx, dec!(0));
        assert!(bar(s.as_mut(), &mut ctx, 5, "80").is_empty());
    }
}
