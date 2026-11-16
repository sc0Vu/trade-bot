//! `macd_cross`: one coin, long while the MACD line is above its signal
//! line, cash otherwise.
//!
//! MACD line = EMA(`fast_period`) − EMA(`slow_period`) of closes; signal =
//! EMA(`signal_period`) of the MACD line (Appel's 12 / 26 / 9 by default
//! in the example). Flat and MACD > signal → buy; long and MACD < signal →
//! sell; equal → nothing. A level rule like `dual_ma_cross`, not a
//! crossing event: the first bar with enough history buys at once if MACD
//! is already on top.
//!
//! The EMAs are computed over a fixed window of the newest
//! `3·slow_period + signal_period` closes, each seeded with a simple mean
//! (`indicator::ema_series`): an EMA run over the whole history would shift
//! with where the bounded history starts. At 12 / 26 / 9 the slow seed is
//! discounted to under 1% by the window's end. Needs that many closed
//! candles; `history_len` must be at least that.
//!
//! What it bets: MACD is a difference of two trend averages, so the rule
//! is a momentum filter that turns earlier than `dual_ma_cross` on the
//! same periods (the signal line reacts to the MACD's own slope). Earlier
//! means more whipsaws in ranges, each a round trip of fees and slippage,
//! and it still lags turns. Expected edge is unverified.
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5), in `Decimal` (invariant 1).

use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "macd_cross";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MacdCrossParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Fast EMA period (`12`). At least 1, below `slow_period`.
    fast_period: usize,
    /// Slow EMA period (`26`).
    slow_period: usize,
    /// EMA period of the MACD line that makes the signal line (`9`). At
    /// least 1.
    signal_period: usize,
    /// Quote spent on each buy (`"1000"` USDT), as a quote-budget market
    /// order.
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
pub struct MacdCross {
    p: MacdCrossParams,
}

impl MacdCross {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: MacdCrossParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.fast_period == 0 || p.fast_period >= p.slow_period {
            return bad("need 1 <= fast_period < slow_period");
        }
        if p.signal_period == 0 {
            return bad("signal_period must be >= 1");
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

    /// Closes the EMAs run over: see the module docs.
    fn window(&self) -> usize {
        3 * self.p.slow_period + self.p.signal_period
    }

    /// (MACD, signal) on the current bar; `None` until `window()` closes
    /// exist.
    fn macd(&self, ctx: &Context) -> Option<(Decimal, Decimal)> {
        let n = self.window();
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n {
            return None;
        }
        let mut closes: Vec<Decimal> = hist.rev().take(n).map(|c| c.close.get()).collect();
        closes.reverse();
        let fast = indicator::ema_series(&closes, self.p.fast_period);
        let slow = indicator::ema_series(&closes, self.p.slow_period);
        // fast[i] ends at closes[fast − 1 + i]; align it with slow's start.
        let skip = self.p.slow_period - self.p.fast_period;
        let line: Vec<Decimal> = fast[skip..]
            .iter()
            .zip(&slow)
            .map(|(f, s)| *f - *s)
            .collect();
        let signal = indicator::ema_series(&line, self.p.signal_period);
        Some((*line.last()?, *signal.last()?))
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
    StrategyRegistration { name: NAME, build: MacdCross::build }
}

impl Strategy for MacdCross {
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
        if c.instrument != *coin || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        let Some((macd, signal)) = self.macd(ctx) else {
            return Vec::new();
        };
        match self.long_units(ctx, c) {
            None if macd > signal => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if macd < signal => vec![Signal::Submit {
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
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{Order, OrderId, OrderSpec, Position, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(fast: usize, slow: usize, signal: usize) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               fast_period = {fast}
               slow_period = {slow}
               signal_period = {signal}
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    /// fast 1, slow 2, signal 2: window 3·2 + 2 = 8 closes.
    fn params() -> Params {
        params_with(1, 2, 2)
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(), 20)
    }

    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
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

    fn macd_after(prices: &[&str]) -> Option<(Decimal, Decimal)> {
        let s = MacdCross {
            p: params().parse(NAME).unwrap(),
        };
        let mut ctx = ctx();
        for (i, p) in prices.iter().enumerate() {
            driver::push_candle(&mut ctx, candle(i as i64, p));
        }
        s.macd(&ctx)
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with(0, 2, 2)).is_err());
        assert!(build(NAME, &params_with(2, 2, 2)).is_err());
        assert!(build(NAME, &params_with(3, 2, 2)).is_err());
        assert!(build(NAME, &params_with(1, 2, 0)).is_err());
    }

    /// fast 1 (EMA = the close), slow 2 (α 2/3), signal 2, window 8.
    /// Closes 100 x7, then 103:
    ///   slow EMA: seed mean(100, 100) = 100, flat until the last step:
    ///   100 + 2/3·3 = 102. MACD line: 0 x6, then 103 − 102 = 1.
    ///   signal: seed mean(0, 0) = 0, flat, last 0 + 2/3·1 = 2/3.
    /// MACD 1 > signal 2/3.
    #[test]
    fn macd_and_signal_by_hand() {
        let mut p = vec!["100"; 7];
        assert_eq!(macd_after(&p), None, "7 closes < window 8");
        p.push("103");
        let (m, s) = macd_after(&p).unwrap();
        assert_eq!(m, dec!(1));
        assert_eq!(s.round_dp(20), (dec!(2) / dec!(3)).round_dp(20));
    }

    /// Flat history: MACD = signal = 0, no decision either way. A jump up
    /// buys (above); held, a drop sells: closes ... 103, 100 → slow EMA
    /// 102 + 2/3·(100 − 102) = 100.667, MACD −0.667; signal 2/3 + 2/3·
    /// (−0.667 − 0.667) = −0.222; −0.667 < −0.222 → sell.
    #[test]
    fn buys_above_the_signal_and_sells_below() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        for i in 0..7 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 7, "103"), vec![buy()]);
        hold(&mut ctx, dec!(9.7));
        assert_eq!(bar(s.as_mut(), &mut ctx, 8, "100"), vec![sell(dec!(9.7))]);
    }

    #[test]
    fn flat_prices_decide_nothing() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(1));
        for i in 0..12 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        for i in 0..7 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        assert!(bar(s.as_mut(), &mut ctx, 7, "103").is_empty());
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
