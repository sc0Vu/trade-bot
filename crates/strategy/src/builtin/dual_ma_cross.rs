//! `dual_ma_cross`: one coin, long while a fast moving average is above a
//! slow one, cash otherwise.
//!
//! The classic two-average trend filter, and a quicker-reacting variant of
//! `sma_trend`: instead of comparing the close with one average, it
//! compares a short average (`fast_period` closes) with a long one
//! (`slow_period` closes), which filters out single-bar spikes. The rule is
//! a level, not an event: flat and `fast > slow` buys, long and `fast <
//! slow` sells, equal does nothing. So the first bar with enough history
//! buys at once if the fast average is already on top; it doesn't wait to
//! see a cross happen. Cash is the defensive position (v1 spot can't
//! short).
//!
//! It bets on trends that last longer than the slow window. It fails in
//! ranges: the averages cross back and forth, each flip paying a round
//! trip, and every signal arrives late by construction (an average lags
//! price), so it buys after the rise and sells after the fall. Expected
//! edge is unverified.
//!
//! Both averages include the current, closed bar and nothing later
//! (invariant 2), read through `Context` (invariant 5), in `Decimal`
//! (invariant 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "dual_ma_cross";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DualMaCrossParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Closes in the fast average, current bar included. Must be less than
    /// `slow_period`.
    fast_period: usize,
    /// Closes in the slow average. Nothing is decided until this many have
    /// closed, so the engine's `history_len` must be at least this.
    slow_period: usize,
    /// Quote spent each time the strategy goes long (`"1000"` USDT), as a
    /// quote-budget market buy.
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
pub struct DualMaCross {
    p: DualMaCrossParams,
}

impl DualMaCross {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: DualMaCrossParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.fast_period == 0 || p.fast_period >= p.slow_period {
            return bad("need 1 <= fast_period < slow_period");
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

    /// Mean of the last `n` closes, ending at the current bar; `None` until
    /// that many have closed.
    fn sma(ctx: &Context, coin: &InstrumentId, n: usize) -> Option<Decimal> {
        let hist = ctx.history(coin);
        if hist.len() < n {
            return None;
        }
        let sum: Decimal = hist.rev().take(n).map(|c| c.close.get()).sum();
        Some(sum / Decimal::from(n))
    }

    /// Lot-rounded units held, if a sell of them would pass the venue's
    /// filters at `c`'s close. `None` is cash, including dust a buy's base
    /// fee left behind.
    fn long_units(&self, ctx: &Context, c: &Candle) -> Option<Qty> {
        let inst = ctx.instrument(&self.p.coin)?;
        let units = inst.round_qty(ctx.position(&self.p.coin).qty());
        inst.check_order(units, c.close).ok().map(|()| units)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: DualMaCross::build }
}

impl Strategy for DualMaCross {
    fn name(&self) -> &str {
        NAME
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        // An order still working would be doubled by deciding again.
        if c.instrument != *coin || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        let (Some(fast), Some(slow)) = (
            Self::sma(ctx, coin, self.p.fast_period),
            Self::sma(ctx, coin, self.p.slow_period),
        ) else {
            return Vec::new();
        };
        match self.long_units(ctx, c) {
            None if fast > slow => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if fast < slow => vec![Signal::Submit {
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

    fn params_with(fast: usize, slow: usize) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               fast_period = {fast}
               slow_period = {slow}
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    fn params() -> Params {
        params_with(2, 4)
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(), 10)
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

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    /// fast must be below slow, and at least 1: refused at load.
    #[test]
    fn fast_must_be_shorter_than_slow() {
        for (fast, slow) in [(4, 4), (5, 4), (0, 4)] {
            assert!(
                build(NAME, &params_with(fast, slow)).is_err(),
                "{fast} {slow}"
            );
        }
        let float_fee = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               fast_period = 2
               slow_period = 4
               quote_amount = "1000"
               fee = 0.001
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &float_fee).is_err());
    }

    /// slow_period 4: bars 0-2 can't form the slow average, however steep
    /// the rise.
    #[test]
    fn insufficient_history_means_no_signal() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        assert!(bar(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "200").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 2, "300").is_empty());
        // Bar 3: fast (300 + 400)/2 = 350 > slow 1000/4 = 250.
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "400"), vec![buy()]);
    }

    /// fast_period 2, slow_period 4.
    ///   100 x4: fast = slow = 100, nothing.
    ///   110: fast (100+110)/2 = 105 > slow (100+100+100+110)/4 = 102.5: buy.
    ///   Held. 120: fast 115 > slow 107.5: hold.
    ///   90: fast (120+90)/2 = 105 = slow (100+110+120+90)/4 = 105: nothing.
    ///   80: fast 85 < slow (110+120+90+80)/4 = 100: sell.
    #[test]
    fn long_above_and_cash_below() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        for i in 0..4 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 4, "110"), vec![buy()]);
        hold(&mut ctx, dec!(9));
        assert!(bar(s.as_mut(), &mut ctx, 5, "120").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 6, "90").is_empty());
        assert_eq!(bar(s.as_mut(), &mut ctx, 7, "80"), vec![sell(dec!(9))]);
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        for (i, p) in [(0, "100"), (1, "100"), (2, "100")] {
            bar(s.as_mut(), &mut ctx, i, p);
        }
        // fast (100+120)/2 = 110 > slow 105: the dust doesn't count as long.
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "120"), vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params()).unwrap();
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
        assert!(bar(s.as_mut(), &mut ctx, 3, "200").is_empty());
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               fast_period = 1
               slow_period = 2
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
}
