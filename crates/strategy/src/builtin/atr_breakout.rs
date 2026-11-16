//! `atr_breakout`: one coin, buy a close that clears its average by a
//! multiple of the average true range, sell when it falls a multiple of
//! the ATR below the recent high.
//!
//! Over the last `period` bars (current included): `mid` = mean close,
//! `atr` = [`indicator::atr`] (needs `period + 1`
//! candles), `peak` = highest high. Flat and close > mid + `entry_mult`·atr
//! → buy. Long and close < peak − `exit_mult`·atr → sell everything to
//! cash (v1 spot can't short). Entry is a Keltner-style upper band, exit a
//! chandelier stop (LeBeau).
//!
//! Why ATR rather than a fixed percentage: the governing constraint is
//! noise. A threshold inside a bar's ordinary range is crossed by noise
//! alone, paying a round trip (two fees, slippage on both market legs) for
//! nothing; a fixed percentage is too tight when the coin is volatile and
//! too loose when it's calm. Scaling by ATR keeps the entry a move larger
//! than recent noise and the stop outside it.
//!
//! What it costs: the exit gives back `exit_mult`·ATR from the peak plus
//! the gap to the next open (decided on the close, filled at the next open),
//! so a trend has to run further than that plus costs to pay. The stop
//! isn't a ratchet: as the old high leaves the window, or ATR widens, it
//! can move down, as LeBeau's original does. It can't tell a breakout from
//! a volatility spike inside a range; each such false breakout loses about
//! (`entry_mult` + `exit_mult`)·ATR plus costs. Expected edge is
//! unverified.
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5), in `Decimal` (invariant 1).

use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "atr_breakout";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AtrBreakoutParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Bars in the average, the ATR and the peak, current included. Needs
    /// `period + 1` closed candles (the ATR's first true range needs the
    /// close before), so the engine's `history_len` must be at least that.
    period: usize,
    /// ATRs above the average a close must be to buy (`"1"`). `"0"` buys
    /// any close above the average.
    #[serde(deserialize_with = "exact_decimal")]
    entry_mult: Decimal,
    /// ATRs below the window's highest high a close must fall to sell
    /// (`"3"`). Positive: at 0 any close under the high would sell.
    #[serde(deserialize_with = "exact_decimal")]
    exit_mult: Decimal,
    /// Quote spent on each entry (`"1000"` USDT), as a quote-budget market
    /// order: never more than this is spent.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required, like the other
    /// strategies', so the config states the costs the backtest runs with;
    /// decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`). Required
    /// and unused in decisions, as `fee`; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct AtrBreakout {
    p: AtrBreakoutParams,
}

/// What the window says on the current bar.
struct Levels {
    /// close > this → buy (flat).
    entry: Decimal,
    /// close < this → sell (long).
    stop: Decimal,
}

impl AtrBreakout {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: AtrBreakoutParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.period == 0 {
            return bad("period must be >= 1");
        }
        if p.entry_mult < Decimal::ZERO {
            return bad("entry_mult must be >= 0");
        }
        if p.exit_mult <= Decimal::ZERO {
            return bad("exit_mult must be positive");
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
        Ok(Box::new(Self { p }))
    }

    /// Entry and stop levels on the current bar; `None` until `period + 1`
    /// candles have closed.
    fn levels(&self, ctx: &Context) -> Option<Levels> {
        let n = self.p.period;
        let atr = indicator::atr(ctx.history(&self.p.coin), n)?;
        let window: Vec<&Candle> = ctx.history(&self.p.coin).rev().take(n).collect();
        let mid = window.iter().map(|c| c.close.get()).sum::<Decimal>() / Decimal::from(n);
        let peak = window.iter().map(|c| c.high).max()?.get();
        Some(Levels {
            entry: mid + self.p.entry_mult * atr,
            stop: peak - self.p.exit_mult * atr,
        })
    }

    /// Lot-rounded units held, if a sell of them would pass the venue's
    /// filters at `c`'s close. `None` is cash, including dust a buy's base
    /// fee left behind.
    fn long_units(&self, ctx: &Context, c: &Candle) -> Option<Qty> {
        let inst = ctx.instrument(&self.p.coin)?;
        let units = inst.round_qty(ctx.position(&self.p.coin).qty());
        inst.check_market(units, c.close, Side::Sell)
            .ok()
            .map(|()| units)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: AtrBreakout::build }
}

impl Strategy for AtrBreakout {
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
        // An order still working would be doubled by deciding again.
        if c.instrument != *coin || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        let Some(l) = self.levels(ctx) else {
            return Vec::new();
        };
        let close = c.close.get();
        match self.long_units(ctx, c) {
            None if close > l.entry => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if close < l.stop => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Sell,
                size: Size::Base(units),
                kind: OrderKind::Market,
            }],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::btcusdt;
    use crate::{build, driver, names};
    use bot_core::{CandleSpec, Interval, Order, OrderId, OrderSpec, Position, Price, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(period: usize, entry: &str, exit: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               period = {period}
               entry_mult = "{entry}"
               exit_mult = "{exit}"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    fn params() -> Params {
        params_with(3, "0.5", "1")
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(), 10)
    }

    /// Candle `i` with distinct open/high/low/close.
    fn ohlc(i: i64, o: &str, h: &str, l: &str, c: &str) -> Candle {
        let p = |s: &str| s.parse::<Price>().unwrap();
        Candle::new(CandleSpec {
            instrument: btcusdt().id.clone(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(i * 3_600_000),
            close_time: Timestamp::from_millis(i * 3_600_000 + 3_599_999),
            open: p(o),
            high: p(h),
            low: p(l),
            close: p(c),
            volume: Qty::new(dec!(1)),
            quote_volume: Notional::new(p(c).get()),
        })
        .unwrap()
    }

    /// A quiet bar at 100: range 99..101, true range 2.
    fn quiet(i: i64) -> Candle {
        ohlc(i, "100", "101", "99", "100")
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, c: Candle) -> Vec<Signal> {
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
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

    fn buy() -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
        }
    }

    fn sell(qty: Decimal) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
        }
    }

    /// Four quiet bars, then whatever the test feeds.
    fn warmed(s: &mut dyn Strategy, ctx: &mut Context) {
        for i in 0..4 {
            assert!(feed(s, ctx, quiet(i)).is_empty(), "bar {i}");
        }
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with(0, "1", "1")).is_err());
        assert!(build(NAME, &params_with(3, "-0.1", "1")).is_err());
        assert!(build(NAME, &params_with(3, "1", "0")).is_err());
        assert!(build(NAME, &params_with(3, "0", "1")).is_ok());
        let float_mult = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               period = 3
               entry_mult = 1
               exit_mult = "1"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &float_mult).is_err());
    }

    /// period 3 needs 4 candles (3 true ranges). Bars 0-2 decide nothing
    /// however far they run; bar 3 closes at 150 over an average of
    /// (110 + 130 + 150) / 3 = 130 with ATR (20 + 20 + 20) / 3 = 20: entry
    /// 130 + 0.5·20 = 140 < 150, buy.
    #[test]
    fn insufficient_history_means_no_signal() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        for (i, p) in [(0, "90"), (1, "110"), (2, "130")] {
            assert!(feed(s.as_mut(), &mut ctx, ohlc(i, p, p, p, p)).is_empty());
        }
        assert_eq!(
            feed(s.as_mut(), &mut ctx, ohlc(3, "150", "150", "150", "150")),
            vec![buy()]
        );
    }

    /// After four quiet bars (ATR 2), bar 4 runs 100 → 107, closes c.
    ///   TR = max(7, |107-100|, |100-100|) = 7; ATR = (2 + 2 + 7) / 3 = 11/3.
    ///   mid = (100 + 100 + c) / 3.
    /// entry_mult 0.5: entry = mid + 11/6.
    ///   c = 105: mid 305/3 = 101.67, entry 103.5 < 105 → buy.
    ///   c = 103: mid 101, entry 102.83 < 103 → buy.
    ///   c = 102.5: mid 100.83, entry 102.67 > 102.5 → nothing.
    /// entry_mult 1, c = 103: entry 101 + 11/3 = 104.67 > 103 → nothing, so
    /// the multiple, not just the average, sets the bar.
    #[test]
    fn buys_a_close_clear_of_the_average_by_entry_mult_atrs() {
        for (entry, close, buys) in [
            ("0.5", "105", true),
            ("0.5", "103", true),
            ("0.5", "102.5", false),
            ("1", "103", false),
        ] {
            let mut s = build(NAME, &params_with(3, entry, "1")).unwrap();
            let mut ctx = ctx();
            warmed(s.as_mut(), &mut ctx);
            let got = feed(s.as_mut(), &mut ctx, ohlc(4, "100", "107", "100", close));
            let want = if buys { vec![buy()] } else { vec![] };
            assert_eq!(got, want, "entry_mult {entry}, close {close}");
        }
    }

    /// Flat prices: ATR 0, entry = the average itself, and a close equal to
    /// it is not above it.
    #[test]
    fn no_volatility_no_entry() {
        let mut s = build(NAME, &params_with(3, "0", "1")).unwrap();
        let mut ctx = ctx();
        for i in 0..6 {
            let c = ohlc(i, "100", "100", "100", "100");
            assert!(feed(s.as_mut(), &mut ctx, c).is_empty(), "bar {i}");
        }
    }

    /// Long from bar 4. Window = the last 3 bars; exit_mult as given.
    ///   bar 4: 100 → 107 close 106. TRs 2, 2, 7.
    ///   bar 5: prev 106, 105..110 close 109: TR max(5, 4, 1) = 5.
    ///     ATR (2 + 7 + 5)/3 = 14/3, peak 110.
    ///     exit 1: stop 110 - 4.67 = 105.33 < 109 → hold.
    ///   bar 6: prev 109, 108..116 close 115: TR max(8, 7, 1) = 8.
    ///     ATR (7 + 5 + 8)/3 = 20/3, peak 116, stop 109.33 < 115 → hold.
    ///   bar 7: prev 115, 107..115 close 108: TR max(8, 0, 8) = 8.
    ///     ATR (5 + 8 + 8)/3 = 7, peak 116, stop 109 > 108 → sell.
    /// With exit_mult 2 the stop at bar 7 is 116 - 14 = 102 < 108: hold.
    #[test]
    fn sells_a_close_exit_mult_atrs_below_the_peak() {
        for (exit, sells) in [("1", true), ("2", false)] {
            let mut s = build(NAME, &params_with(3, "0.5", exit)).unwrap();
            let mut ctx = ctx();
            warmed(s.as_mut(), &mut ctx);
            let entry = feed(s.as_mut(), &mut ctx, ohlc(4, "100", "107", "100", "106"));
            assert_eq!(entry, vec![buy()]);
            hold(&mut ctx, dec!(9));
            for c in [
                ohlc(5, "106", "110", "105", "109"),
                ohlc(6, "109", "116", "108", "115"),
            ] {
                assert!(feed(s.as_mut(), &mut ctx, c).is_empty(), "exit {exit}");
            }
            let got = feed(s.as_mut(), &mut ctx, ohlc(7, "115", "115", "107", "108"));
            let want = if sells { vec![sell(dec!(9))] } else { vec![] };
            assert_eq!(got, want, "exit_mult {exit}");
        }
    }

    /// The peak is the window's, not the trade's: once a spike leaves the
    /// window the stop drops with it. Exit 1, period 3, four quiet bars, then
    ///   bar 4: prev 100, 100..130 close 120: TR 30.
    ///   bars 5-7: 99..101 close 100; bar 5's TR vs 120 = max(2, 19, 21) = 21.
    ///   bar 6: TRs 30, 21, 2 → ATR 53/3, peak 130 (bar 4), stop 130 - 53/3
    ///     = 112.33: a close at 100 sells.
    ///   bar 7: TRs 21, 2, 2 → ATR 25/3, peak 101 (bar 4 gone), stop
    ///     101 - 25/3 = 92.67: the same close holds.
    #[test]
    fn the_stop_follows_the_window_not_the_trade() {
        let s = AtrBreakout {
            p: params().parse(NAME).unwrap(),
        };
        let mut ctx = ctx();
        for i in 0..4 {
            driver::push_candle(&mut ctx, quiet(i));
        }
        driver::push_candle(&mut ctx, ohlc(4, "100", "130", "100", "120"));
        driver::push_candle(&mut ctx, quiet(5));
        driver::push_candle(&mut ctx, quiet(6));
        let stop6 = s.levels(&ctx).unwrap().stop;
        assert_eq!(stop6, dec!(130) - dec!(53) / dec!(3));
        driver::push_candle(&mut ctx, quiet(7));
        let stop7 = s.levels(&ctx).unwrap().stop;
        assert_eq!(stop7, dec!(101) - dec!(25) / dec!(3));
        assert!(stop6 > dec!(100) && dec!(100) > stop7);
    }

    fn working() -> Order {
        Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        warmed(s.as_mut(), &mut ctx);
        let got = feed(s.as_mut(), &mut ctx, ohlc(4, "100", "107", "100", "106"));
        assert_eq!(got, vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        warmed(s.as_mut(), &mut ctx);
        driver::set_open_orders(&mut ctx, vec![working()]);
        let got = feed(s.as_mut(), &mut ctx, ohlc(4, "100", "107", "100", "106"));
        assert!(got.is_empty());
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               period = 1
               entry_mult = "0"
               exit_mult = "1"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &p).unwrap();
        let mut ctx = ctx();
        for (i, price) in [(0, "100"), (1, "200"), (2, "300")] {
            let c = ohlc(i, price, price, price, price);
            assert!(feed(s.as_mut(), &mut ctx, c).is_empty());
        }
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params()).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = ctx();
        assert!(s.restore_state(state, &ctx).is_ok());
        assert!(
            s.restore_state(serde_json::json!({ "v": 2 }), &ctx)
                .is_err()
        );
    }
}
