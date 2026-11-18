//! `turn_of_month`: hold one instrument over the turn of each month, cash
//! the rest of the time.
//!
//! Equity returns have clustered around month ends: the last trading day
//! and the first few of the next month (Ariel 1987; Lakonishok and Smidt
//! 1988; McConnell and Xu 2008 found it in most markets through 2005),
//! often explained by month-end pay and fund flows. A calendar rule, not a
//! price forecast: few trades a month, so a fixed per-order fee (eToro's
//! $1) costs little. Published anomalies tend to shrink after publication;
//! the effect in your market and years is what a backtest is for. Edge
//! unverified here.
//!
//! The bot can't know at a close whether the next bar is still in this
//! month (weekends, holidays), so the window is defined from what is known:
//!
//! - **enter**: a bar closing in the last `enter_days` calendar days of
//!   its month (UTC; a bar's date is its close time's) → long;
//! - **hold**: through the first `hold_bars` bars closing in the new month;
//!   the close of bar number `hold_bars` sells.
//!
//! Outside the window → cash. Stateless: the window comes from the bar's
//! date and the history (bars of the month so far), so a restart, or a
//! first start inside a window, simply holds. The engine's `history_len`
//! must cover `hold_bars` bars. Same single-coin skeleton as `sma_trend`:
//! quote-budget market buys, whole lot-rounded sells, dust = cash, no
//! decision while an order is open.

use super::calendar::{Period, day_of_month, period_start};
use super::long_flat;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "turn_of_month";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnOfMonthParams {
    /// The one instrument traded (`"etoro:SPY"`).
    coin: InstrumentId,
    /// Calendar days at the end of the month whose bars buy (`2`: the last
    /// two days). 1 to 10.
    enter_days: i64,
    /// Bars of the new month held; the close of the last sells (`3`).
    /// 1 to 15.
    hold_bars: usize,
    /// Quote spent on each buy (`"1000"`), as a quote-budget market order.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required so the config states
    /// the costs; decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused, as `fee`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct TurnOfMonth {
    p: TurnOfMonthParams,
}

impl TurnOfMonth {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: TurnOfMonthParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !(1..=10).contains(&p.enter_days) {
            return bad("enter_days must be 1 to 10");
        }
        if !(1..=15).contains(&p.hold_bars) {
            return bad("hold_bars must be 1 to 15");
        }
        long_flat::check_common(NAME, p.quote_amount, p.fee, p.slippage_bps)?;
        Ok(Box::new(Self { p }))
    }

    /// Whether the bar `c` falls in the window: the month's last
    /// `enter_days` days, or the new month before its `hold_bars`-th bar.
    /// "Before" needs the history to reach back into the previous month:
    /// a history that starts mid-month (a short warm-up) can't tell its
    /// first bar from the month's first, and reads as outside.
    fn in_window(&self, ctx: &Context, c: &Candle) -> bool {
        let (day, days) = day_of_month(c.close_time);
        if day > days - self.p.enter_days {
            return true;
        }
        let month = period_start(Period::Month, c.close_time);
        let mut so_far = 0;
        for b in ctx.history(&self.p.coin).rev() {
            if period_start(Period::Month, b.close_time) != month {
                // The turn is in the history: these are the month's first.
                return so_far < self.p.hold_bars;
            }
            so_far += 1;
            if so_far >= self.p.hold_bars {
                return false;
            }
        }
        false
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: TurnOfMonth::build }
}

impl Strategy for TurnOfMonth {
    fn name(&self) -> &str {
        NAME
    }

    /// The window comes from dates and history, rebuilt on a restart: only
    /// the format version is saved.
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
        let inside = self.in_window(ctx, c);
        long_flat::decide(ctx, coin, c, self.p.quote_amount, inside, !inside)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{CandleSpec, Interval, OrderKind, Position, Qty, Side, Size, Timestamp};
    use rust_decimal_macros::dec;

    const DAY: i64 = 86_400_000;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    /// A daily bar for day `i` after 1970-01-01 (day 0 = Jan 1).
    fn day(i: i64) -> Candle {
        Candle::new(CandleSpec {
            interval: Interval::D1,
            open_time: Timestamp::from_millis(i * DAY),
            close_time: Timestamp::from_millis(i * DAY + DAY - 1),
            ..candle(0, "100").into()
        })
        .unwrap()
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, i: i64) -> Vec<Signal> {
        let c = day(i);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        s.on_candle(ctx, &c)
    }

    /// A history that starts on Feb 1 can't tell it's the month's first
    /// bar: not bought. One that starts Jan 31 (in the late window) holds
    /// into February as the turn shows.
    #[test]
    fn a_history_starting_mid_window_needs_the_turn_to_count() {
        let mut s = build(NAME, &params("enter_days = 1\nhold_bars = 3")).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 40);
        assert!(
            feed(s.as_mut(), &mut ctx, 31).is_empty(),
            "Feb 1, no turn seen"
        );
        let mut s = build(NAME, &params("enter_days = 1\nhold_bars = 3")).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 40);
        assert_eq!(feed(s.as_mut(), &mut ctx, 30).len(), 1, "Jan 31 buys");
        assert_eq!(
            feed(s.as_mut(), &mut ctx, 31).len(),
            1,
            "flat on Feb 1: buys"
        );
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        let ok = params("enter_days = 2\nhold_bars = 3");
        assert_eq!(build(NAME, &ok).unwrap().name(), NAME);
        for bad in [
            "enter_days = 0\nhold_bars = 3",
            "enter_days = 11\nhold_bars = 3",
            "enter_days = 2\nhold_bars = 0",
            "enter_days = 2\nhold_bars = 16",
        ] {
            assert!(build(NAME, &params(bad)).is_err(), "{bad}");
        }
    }

    /// enter_days 2, hold_bars 2, January 1970 (31 days): Jan 29 is out
    /// (and, as the first bar in history, not taken for a month's first);
    /// Jan 30 (day 30 > 31 − 2) buys; Jan 31 and Feb 1 (bar 1 of February)
    /// hold; Feb 2 (bar 2) sells the 10 held. February 1970 has 28 days:
    /// Feb 26 is out, Feb 27 buys again.
    #[test]
    fn holds_over_the_turn_of_each_month() {
        let mut s = build(NAME, &params("enter_days = 2\nhold_bars = 2")).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 40);
        let buy = vec![Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
        }];
        assert!(feed(s.as_mut(), &mut ctx, 28).is_empty(), "Jan 29");
        assert_eq!(feed(s.as_mut(), &mut ctx, 29), buy, "Jan 30");
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(dec!(10)),
                Notional::new(dec!(1000)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        assert!(feed(s.as_mut(), &mut ctx, 30).is_empty(), "Jan 31");
        assert!(feed(s.as_mut(), &mut ctx, 31).is_empty(), "Feb 1");
        assert_eq!(
            feed(s.as_mut(), &mut ctx, 32),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(10))),
                kind: OrderKind::Market,
            }],
            "Feb 2"
        );
        driver::set_position(&mut ctx, Position::flat(btcusdt().id.clone()));
        for i in 33..56 {
            assert!(feed(s.as_mut(), &mut ctx, i).is_empty(), "day {i}");
        }
        assert!(feed(s.as_mut(), &mut ctx, 56).is_empty(), "Feb 26");
        assert_eq!(feed(s.as_mut(), &mut ctx, 57), buy, "Feb 27");
    }
}
