//! `donchian_breakout`: one coin, buy a breakout above the recent range,
//! sell a breakdown below it.
//!
//! The channel is the highest high and lowest low of the `channel_period`
//! candles *before* the current one. Flat and the close is above the
//! channel's high: buy. Long and the close is below the channel's low: sell
//! everything to cash. Cash is the defensive position (v1 spot can't
//! short).
//!
//! The current bar is left out of its own channel. That isn't what keeps
//! out lookahead (the current bar has closed, so its high is known), but
//! without it the rule could never fire: a close can't exceed its own bar's
//! high. So "a breakout" means the close beat every earlier bar's high.
//!
//! It bets that breakouts continue: a price leaving its range starts a
//! trend. It whipsaws in ranges: false breakouts buy the top of the range,
//! and because the exit is the opposite side of the channel, a failed
//! breakout gives back the whole range width plus a round trip of costs.
//! Expected edge is unverified.
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5), in `Decimal` (invariant 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, InstrumentId, Notional, OrderKind, Price, Qty, Side, Signal, Size,
};
use serde::Deserialize;

const NAME: &str = "donchian_breakout";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DonchianBreakoutParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Earlier candles the channel spans, current bar excluded. Needs
    /// `channel_period + 1` closed candles before anything is decided, so
    /// the engine's `history_len` must be at least that.
    channel_period: usize,
    /// Quote spent on each breakout buy (`"1000"` USDT), as a quote-budget
    /// market order.
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
pub struct DonchianBreakout {
    p: DonchianBreakoutParams,
}

impl DonchianBreakout {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: DonchianBreakoutParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.channel_period == 0 {
            return bad("channel_period must be >= 1");
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

    /// (highest high, lowest low) of the `channel_period` candles before the
    /// current one; `None` until that many exist.
    fn channel(&self, ctx: &Context) -> Option<(Price, Price)> {
        let n = self.p.channel_period;
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n + 1 {
            return None;
        }
        // Newest first; skip the current bar.
        let prior: Vec<&Candle> = hist.rev().skip(1).take(n).collect();
        let high = prior.iter().map(|c| c.high).max()?;
        let low = prior.iter().map(|c| c.low).min()?;
        Some((high, low))
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
    StrategyRegistration { name: NAME, build: DonchianBreakout::build }
}

impl Strategy for DonchianBreakout {
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
        let Some((high, low)) = self.channel(ctx) else {
            return Vec::new();
        };
        match self.long_units(ctx, c) {
            None if c.close > high => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if c.close < low => vec![Signal::Submit {
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
    use bot_core::{CandleSpec, Interval, Order, OrderId, OrderSpec, Position, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(n: usize) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               channel_period = {n}
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params_with(3), 10)
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, c: Candle) -> Vec<Signal> {
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        feed(s, ctx, candle(i, price))
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

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params_with(3)).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with(0)).is_err());
        let float_fee = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               channel_period = 3
               quote_amount = "1000"
               fee = 0.001
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &float_fee).is_err());
    }

    /// channel_period 3 needs 3 earlier candles plus the current one: bars
    /// 0-2 decide nothing, however high they close.
    #[test]
    fn insufficient_history_means_no_signal() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        assert!(bar(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "200").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 2, "300").is_empty());
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "400"), vec![buy()]);
    }

    /// The current bar is not in its own channel. Bar 3 has a high of 120
    /// but closes at 110: against the earlier highs (100) that is a
    /// breakout. If its own high counted, 110 < 120 could never break out.
    #[test]
    fn current_bar_is_excluded_from_its_channel() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        for i in 0..3 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        assert_eq!(
            feed(s.as_mut(), &mut ctx, ohlc(3, "100", "120", "99", "110")),
            vec![buy()]
        );
    }

    /// Flat candles, channel_period 3.
    ///   100, 100, 100, then 105 > channel high 100: buy.
    ///   Held. 104: inside the channel of (100, 100, 105): hold.
    ///   100: equals the low of (100, 105, 104), not below it: hold.
    ///   99 < the low of (105, 104, 100) = 100: sell.
    #[test]
    fn buys_the_breakout_and_sells_the_breakdown() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        for i in 0..3 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "105"), vec![buy()]);
        hold(&mut ctx, dec!(9));
        assert!(bar(s.as_mut(), &mut ctx, 4, "104").is_empty());
        assert!(
            bar(s.as_mut(), &mut ctx, 5, "100").is_empty(),
            "at the low edge"
        );
        assert_eq!(bar(s.as_mut(), &mut ctx, 6, "99"), vec![sell(dec!(9))]);
    }

    /// Only the last channel_period earlier candles count: the 200 spike at
    /// bar 0 has left a 3-bar channel by bar 4, so 105 breaks out.
    #[test]
    fn old_extremes_roll_out_of_the_channel() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "200");
        for i in 1..4 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 4, "105"), vec![buy()]);
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        for i in 0..3 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "105"), vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let mut ctx = ctx();
        for i in 0..3 {
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
        assert!(bar(s.as_mut(), &mut ctx, 3, "105").is_empty());
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               channel_period = 1
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &p).unwrap();
        let mut ctx = ctx();
        for (i, price) in [(0, "100"), (1, "200"), (2, "300")] {
            assert!(bar(s.as_mut(), &mut ctx, i, price).is_empty());
        }
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with(3)).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = driver::new(vec![btcusdt()], params_with(3), 10);
        assert!(s.restore_state(state, &ctx).is_ok());
        assert!(
            s.restore_state(serde_json::json!({ "v": 2 }), &ctx)
                .is_err()
        );
    }
}
