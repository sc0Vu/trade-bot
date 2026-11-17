//! `stochastic`: one coin, long or flat, on the stochastic oscillator
//! (Lane's %K and %D).
//!
//! %K = where the close sits in the high-low range of the last `k_period`
//! bars (0 at the low, 100 at the high); %D = the mean of the last
//! `d_period` %K values ([`indicator::stochastic`]). Flat and %K crosses
//! above %D (below it on the bar before, above it now) while %K is under
//! `oversold` → buy `quote_amount`; long and %K crosses below %D while
//! above `overbought` → sell everything. Otherwise nothing. Both bars are
//! closed bars in the history: no lookahead, no state.
//!
//! What it bets: a turn from an extreme of the recent range, so it is a
//! mean-reversion rule like `rsi_reversion`. It fails the same way: in a
//! sustained trend the oscillator pins near an extreme, entries come early
//! (a falling knife) and exits late. The crossing requirement makes it
//! trade rarely; with no stop it can hold a long way down. Expected edge
//! is unverified.

use super::long_flat;
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "stochastic";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StochasticParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Bars in %K's range (`14`). At least 1.
    k_period: usize,
    /// %K values in %D (`3`). At least 1. `history_len` must be at least
    /// `k_period + d_period`.
    d_period: usize,
    /// %K below which a cross up buys (`"20"`). In (0, 100).
    #[serde(deserialize_with = "exact_decimal")]
    oversold: Decimal,
    /// %K above which a cross down sells (`"80"`). In (oversold, 100).
    #[serde(deserialize_with = "exact_decimal")]
    overbought: Decimal,
    /// Quote spent on each buy (`"1000"` USDT), as a quote-budget market
    /// order.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required so the config states the
    /// costs the backtest runs with; decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused, as `fee`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct Stochastic {
    p: StochasticParams,
}

impl Stochastic {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: StochasticParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.k_period == 0 || p.d_period == 0 {
            return bad("k_period and d_period must be >= 1");
        }
        let hundred = Decimal::from(100);
        if p.oversold <= Decimal::ZERO || p.oversold >= p.overbought || p.overbought >= hundred {
            return bad("need 0 < oversold < overbought < 100");
        }
        long_flat::check_common(NAME, p.quote_amount, p.fee, p.slippage_bps)?;
        Ok(Box::new(Self { p }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Stochastic::build }
}

impl Strategy for Stochastic {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from history and positions alone, both rebuilt on a
    /// restart: only the format version is saved.
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
        if !long_flat::deciding(ctx, coin, c) {
            return Vec::new();
        }
        let Some(bars) = long_flat::newest(ctx, coin, self.p.k_period + self.p.d_period) else {
            return Vec::new();
        };
        let Some([(k0, d0), (k1, d1)]) =
            indicator::stochastic(&bars, self.p.k_period, self.p.d_period)
        else {
            return Vec::new();
        };
        let crossed_up = k0 <= d0 && k1 > d1;
        let crossed_down = k0 >= d0 && k1 < d1;
        long_flat::decide(
            ctx,
            coin,
            c,
            self.p.quote_amount,
            crossed_up && k1 < self.p.oversold,
            crossed_down && k1 > self.p.overbought,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{OrderKind, Position, Qty, Side, Size};
    use rust_decimal_macros::dec;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               k_period = 2
               d_period = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn levels() -> Params {
        params("oversold = \"30\"\noverbought = \"70\"")
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, prices: &[&str]) -> Vec<Signal> {
        let mut out = Vec::new();
        for (i, p) in prices.iter().enumerate() {
            let c = candle(i as i64, p);
            driver::set_now(ctx, c.close_time);
            driver::push_candle(ctx, c.clone());
            out = s.on_candle(ctx, &c);
        }
        out
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

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &levels()).unwrap().name(), NAME);
        for bad in [
            "oversold = \"70\"\noverbought = \"30\"",
            "oversold = \"0\"\noverbought = \"70\"",
            "oversold = \"30\"\noverbought = \"100\"",
            "oversold = 30\noverbought = \"70\"",
        ] {
            assert!(build(NAME, &params(bad)).is_err(), "{bad}");
        }
    }

    /// Flat bars make %K only 0 or 100. k 2, d 2, oversold 30,
    /// overbought 70. Closes 10, 12, 11, 10, 10.5: %K 100, 0, 0, 100 → a
    /// cross up at %K 100 (prev 0/0, now 100/50), not oversold: no buy.
    /// Long, closes 10, 12, 14, 13: %K 100, 100, 0 → a cross down at %K 0
    /// (prev 100/100, now 0/50), not overbought: no sell.
    #[test]
    fn buys_a_cross_up_from_oversold_and_sells_a_cross_down_from_overbought() {
        let mut s = build(NAME, &levels()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], levels(), 10);
        assert!(feed(s.as_mut(), &mut ctx, &["10", "12", "11", "10", "10.5"]).is_empty());

        let mut s = build(NAME, &levels()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], levels(), 10);
        hold(&mut ctx, dec!(5));
        assert!(feed(s.as_mut(), &mut ctx, &["10", "12", "14", "13"]).is_empty());
    }

    /// Wide bars place the close inside the range. k 2, d 2, oversold 30.
    /// Bars (high, low, close): (100, 80, 81), (90, 70, 71), (80, 60, 61),
    /// then (70, 60, x). %K: 3.33, 3.33, then over (80..60): x = 68 → 40,
    /// x = 65 → 25. prev (3.33, 3.33): %K not above %D. Now %K 40 > %D
    /// 21.67, a cross up but not under 30: no buy; %K 25 > %D 14.17 and
    /// under 30: buy. Checked in Python.
    #[test]
    fn buys_only_when_the_cross_is_in_the_oversold_zone() {
        let bar = |i: i64, h: &str, l: &str, c: &str| {
            let base = candle(i, c);
            bot_core::Candle::new(bot_core::CandleSpec {
                high: h.parse().unwrap(),
                low: l.parse().unwrap(),
                open: c.parse().unwrap(),
                ..base.into()
            })
            .unwrap()
        };
        let run = |last_close: &str| {
            let mut s = build(NAME, &levels()).unwrap();
            let mut ctx = driver::new(vec![btcusdt()], levels(), 10);
            let mut out = Vec::new();
            for c in [
                bar(0, "100", "80", "81"),
                bar(1, "90", "70", "71"),
                bar(2, "80", "60", "61"),
                bar(3, "70", "60", last_close),
            ] {
                driver::set_now(&mut ctx, c.close_time);
                driver::push_candle(&mut ctx, c.clone());
                out = s.on_candle(&mut ctx, &c);
            }
            out
        };
        assert!(run("68").is_empty(), "%K 40: crossed up, not oversold");
        assert_eq!(
            run("65"),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(1000))),
                kind: OrderKind::Market,
            }]
        );
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &levels()).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = driver::new(vec![btcusdt()], levels(), 10);
        assert!(s.restore_state(state, &ctx).is_ok());
    }
}
