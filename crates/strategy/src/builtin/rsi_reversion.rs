//! `rsi_reversion`: buy a dip, sell the bounce. The counterexample to the
//! trend filters.
//!
//! When flat and the coin's RSI falls below `oversold`, buy; when long and
//! RSI rises above `overbought`, sell everything to cash.
//!
//! It bets on a ranging market: a coin pushed down hard tends to snap back.
//! **That bet is dangerous in a trend.** Mean reversion buys dips, so in a
//! sustained downtrend it catches falling knives: it buys the first dip,
//! RSI never recovers to `overbought`, and it holds the coin all the way
//! down. It has no stop and no trend filter by design. It exists for
//! ranging markets, and partly to show, against `sma_trend` and
//! `momentum_rotation`, what betting against the trend costs. Expected edge
//! is unverified.
//!
//! RSI here is the v1 *simple-average* form, not Wilder's smoothing, so a
//! value can be checked by hand. Over the last `rsi_period` close-to-close
//! changes, with G the sum of gains and L the sum of losses:
//! `RSI = 100 * G / (G + L)`. That equals `100 - 100 / (1 + RS)` with
//! `RS = mean gain / mean loss`, since the two means share a divisor. No
//! losses gives 100, no gains gives 0, and no movement at all gives 50
//! (neutral: no signal).
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5) and return `Signal`s; arithmetic is `Decimal` (invariant
//! 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "rsi_reversion";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RsiReversionParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Close-to-close changes the RSI covers. Needs `rsi_period + 1` closes
    /// before anything is decided, so the engine's `history_len` must be at
    /// least that.
    rsi_period: usize,
    /// Buy below this RSI (`"30"`), on the 0-100 scale.
    #[serde(deserialize_with = "exact_decimal")]
    oversold: Decimal,
    /// Sell above this RSI (`"70"`). Must exceed `oversold`.
    #[serde(deserialize_with = "exact_decimal")]
    overbought: Decimal,
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
pub struct RsiReversion {
    p: RsiReversionParams,
}

impl RsiReversion {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: RsiReversionParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        let hundred = Decimal::from(100);
        if p.rsi_period == 0 {
            return bad("rsi_period must be >= 1");
        }
        if p.oversold < Decimal::ZERO || p.overbought > hundred || p.oversold >= p.overbought {
            return bad("need 0 <= oversold < overbought <= 100");
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

    /// Simple-average RSI over the last `rsi_period` changes, ending at the
    /// current close; `None` until `rsi_period + 1` closes exist.
    fn rsi(&self, ctx: &Context) -> Option<Decimal> {
        let n = self.p.rsi_period;
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n + 1 {
            return None;
        }
        let closes: Vec<Decimal> = hist.rev().take(n + 1).map(|c| c.close.get()).collect();
        let (mut gains, mut losses) = (Decimal::ZERO, Decimal::ZERO);
        // `closes` is newest first: each pair is (later, earlier).
        for w in closes.windows(2) {
            let change = w[0] - w[1];
            if change.is_sign_positive() {
                gains += change;
            } else {
                losses -= change;
            }
        }
        let moved = gains + losses;
        if moved.is_zero() {
            return Some(Decimal::from(50));
        }
        Some(Decimal::from(100) * gains / moved)
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
    StrategyRegistration { name: NAME, build: RsiReversion::build }
}

impl Strategy for RsiReversion {
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
        let Some(rsi) = self.rsi(ctx) else {
            return Vec::new();
        };
        match self.long_units(ctx, c) {
            None if rsi < self.p.oversold => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if rsi > self.p.overbought => vec![Signal::Submit {
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

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               rsi_period = 2
               oversold = "30"
               overbought = "70"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(""), 20)
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

    /// The RSI after feeding `closes`, for checking the formula by hand.
    fn rsi_of(closes: &[&str]) -> Option<Decimal> {
        let p = params("");
        let s = RsiReversion {
            p: p.parse(NAME).unwrap(),
        };
        let mut ctx = ctx();
        for (i, c) in closes.iter().enumerate() {
            driver::push_candle(&mut ctx, candle(i as i64, c));
        }
        s.rsi(&ctx)
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        let base = |o: &str, b: &str| {
            Params::from_toml(&format!(
                r#"coin = "binance:BTCUSDT"
                   rsi_period = 2
                   oversold = {o}
                   overbought = {b}
                   quote_amount = "1000"
                   fee = "0.001"
                   slippage_bps = "10""#
            ))
            .unwrap()
        };
        for (o, b) in [
            (r#""70""#, r#""30""#),
            (r#""50""#, r#""50""#),
            (r#""-1""#, r#""70""#),
            (r#""30""#, r#""101""#),
            // A TOML number would pass through a float first (invariant 1).
            ("30", "70"),
        ] {
            assert!(build(NAME, &base(o, b)).is_err(), "{o} {b}");
        }
    }

    /// Hand-worked, rsi_period 2:
    ///   100, 90, 95: changes -10, +5 -> 100 * 5 / 15 = 33.33...
    ///   100, 110, 120: no losses -> 100. 100, 90, 80: no gains -> 0.
    ///   100, 100, 100: no movement -> 50.
    #[test]
    fn rsi_is_the_simple_average_form() {
        assert_eq!(
            rsi_of(&["100", "90", "95"]).unwrap().round_dp(6),
            dec!(33.333333)
        );
        assert_eq!(rsi_of(&["100", "110", "120"]), Some(dec!(100)));
        assert_eq!(rsi_of(&["100", "90", "80"]), Some(dec!(0)));
        assert_eq!(rsi_of(&["100", "100", "100"]), Some(dec!(50)));
        // Only the last rsi_period changes count: the old drop to 50 is gone.
        assert_eq!(rsi_of(&["100", "50", "60", "70"]), Some(dec!(100)));
    }

    /// rsi_period 2 needs 3 closes: two falling bars decide nothing.
    #[test]
    fn insufficient_history_means_no_signal() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        assert!(bar(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "50").is_empty());
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "40"), vec![buy()]);
    }

    /// 100, 100, 90: RSI 0 < 30, buy. Held: 85 (RSI 0), 95 (10/15 =
    /// 66.67, not above 70), 105 (100 > 70) sells.
    #[test]
    fn buys_oversold_and_sells_overbought() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "100");
        bar(s.as_mut(), &mut ctx, 1, "100");
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "90"), vec![buy()]);
        hold(&mut ctx, dec!(9.99));
        assert!(bar(s.as_mut(), &mut ctx, 3, "85").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 4, "95").is_empty());
        assert_eq!(bar(s.as_mut(), &mut ctx, 5, "105"), vec![sell(dec!(9.99))]);
    }

    /// The danger the docs state: in a steady downtrend RSI stays at 0,
    /// never reaches overbought, and the strategy holds the coin all the
    /// way down with no exit.
    #[test]
    fn in_a_downtrend_it_holds_the_falling_knife() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "100");
        bar(s.as_mut(), &mut ctx, 1, "95");
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "90"), vec![buy()]);
        hold(&mut ctx, dec!(9.99));
        for (i, p) in [(3, "80"), (4, "70"), (5, "60"), (6, "50"), (7, "40")] {
            assert!(bar(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        bar(s.as_mut(), &mut ctx, 0, "100");
        bar(s.as_mut(), &mut ctx, 1, "100");
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "90"), vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        bar(s.as_mut(), &mut ctx, 0, "100");
        bar(s.as_mut(), &mut ctx, 1, "100");
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
        assert!(bar(s.as_mut(), &mut ctx, 2, "90").is_empty());
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               rsi_period = 1
               oversold = "30"
               overbought = "70"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &p).unwrap();
        let mut ctx = ctx();
        for (i, price) in [(0, "100"), (1, "50"), (2, "10")] {
            assert!(bar(s.as_mut(), &mut ctx, i, price).is_empty());
        }
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params("")).unwrap();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        let ctx = driver::new(vec![btcusdt()], params(""), 10);
        assert!(s.restore_state(state, &ctx).is_ok());
        assert!(
            s.restore_state(serde_json::json!({ "v": 2 }), &ctx)
                .is_err()
        );
    }
}
