//! `supertrend`: one coin, long while the SuperTrend says up, cash while
//! it says down.
//!
//! SuperTrend (Olivier Seban) draws bands `mult` ATRs above and below the
//! bar's midpoint and lets them only ratchet toward price; a close through
//! the band on the other side flips the trend ([`indicator::supertrend`]).
//! Flat and the trend is up → buy `quote_amount`; long and it is down →
//! sell everything. It is computed over a fixed window of the newest
//! `window` bars, starting the recursion at the window's start, so a
//! backtest, a live run and a restart see the same value on the same bar
//! (a recursion over the whole history would depend on where the bounded
//! history begins); `history_len` must be at least `window`.
//!
//! What it bets: a trend, with an ATR trailing stop as the exit: noise
//! inside `mult` ATRs doesn't flip it, so it trades less than a moving
//! average cross in a choppy market but gives back up to `mult` ATRs at
//! every turn. Like `atr_breakout`, ATR because noise is the constraint.
//! Expected edge is unverified.

use super::long_flat;
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "supertrend";

fn default_window() -> usize {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupertrendParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// True ranges in the ATR (`10`). At least 1.
    period: usize,
    /// Band width in ATRs (`"3"`). Positive.
    #[serde(deserialize_with = "exact_decimal")]
    mult: Decimal,
    /// Newest bars the recursion runs over (`100`, the default). At least
    /// `period + 2`; `history_len` must be at least this.
    #[serde(default = "default_window")]
    window: usize,
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
pub struct Supertrend {
    p: SupertrendParams,
}

impl Supertrend {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: SupertrendParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.period == 0 {
            return bad("period must be >= 1");
        }
        if p.mult <= Decimal::ZERO {
            return bad("mult must be positive");
        }
        if p.window < p.period + 2 {
            return bad("window must be at least period + 2");
        }
        long_flat::check_common(NAME, p.quote_amount, p.fee, p.slippage_bps)?;
        Ok(Box::new(Self { p }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Supertrend::build }
}

impl Strategy for Supertrend {
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
        let Some(bars) = long_flat::newest(ctx, coin, self.p.window) else {
            return Vec::new();
        };
        let Some((up, _)) = indicator::supertrend(&bars, self.p.period, self.p.mult) else {
            return Vec::new();
        };
        long_flat::decide(ctx, coin, c, self.p.quote_amount, up, !up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{CandleSpec, OrderKind, Position, Qty, Side, Size};
    use rust_decimal_macros::dec;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               period = 3
               mult = "2"
               window = 8
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    /// Bar i of the indicator test's climb: close 100 + 2i, high +1, low −2.
    fn climb(i: i64) -> Candle {
        let c = Decimal::from(100 + 2 * i);
        Candle::new(CandleSpec {
            open: bot_core::Price::new(c - dec!(1)),
            high: bot_core::Price::new(c + dec!(1)),
            low: bot_core::Price::new(c - dec!(2)),
            close: bot_core::Price::new(c),
            ..candle(i, "100").into()
        })
        .unwrap()
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, c: Candle) -> Vec<Signal> {
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        s.on_candle(ctx, &c)
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
        for bad in [r#"mult = "0""#, "mult = 2"] {
            let p = Params::from_toml(&format!(
                "coin = \"binance:BTCUSDT\"\nperiod = 3\nwindow = 8\nquote_amount = \"1000\"\n\
                 fee = \"0.001\"\nslippage_bps = \"10\"\n{bad}"
            ))
            .unwrap();
            assert!(build(NAME, &p).is_err(), "{bad}");
        }
        assert!(build(NAME, &params("").clone()).is_ok());
        let p = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nperiod = 3\nmult = \"2\"\nwindow = 4\n\
             quote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"10\"",
        )
        .unwrap();
        assert!(build(NAME, &p).is_err(), "window < period + 2");
    }

    /// The indicator test's climb (up, line 107.5) buys once the 8-bar
    /// window is full; long, the crash to 90 (down) sells everything.
    #[test]
    fn buys_the_up_trend_and_sells_the_flip() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], params(""), 20);
        for i in 0..7 {
            assert!(feed(s.as_mut(), &mut ctx, climb(i)).is_empty(), "bar {i}");
        }
        assert_eq!(
            feed(s.as_mut(), &mut ctx, climb(7)),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(1000))),
                kind: OrderKind::Market,
            }]
        );
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(dec!(8)),
                Notional::new(dec!(1000)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        let crash = Candle::new(CandleSpec {
            open: "114".parse().unwrap(),
            high: "114".parse().unwrap(),
            low: "90".parse().unwrap(),
            close: "90".parse().unwrap(),
            ..candle(8, "100").into()
        })
        .unwrap();
        assert_eq!(
            feed(s.as_mut(), &mut ctx, crash),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(8))),
                kind: OrderKind::Market,
            }]
        );
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params("")).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = driver::new(vec![btcusdt()], params(""), 10);
        assert!(s.restore_state(state, &ctx).is_ok());
    }
}
