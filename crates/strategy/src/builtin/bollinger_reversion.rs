//! `bollinger_reversion`: one coin, buy a close below the lower Bollinger
//! band, sell once it is back above the middle.
//!
//! Over the last `period` closes (current included): `mid` = their mean,
//! `sd` = their population standard deviation (Bollinger's definition,
//! divided by `period`). Flat and close < mid − `k`·sd → buy; long and
//! close > mid → sell everything to cash.
//!
//! The band test needs no square root: close < mid − k·sd exactly when
//! close < mid and (mid − close)² > k²·variance, both exact in `Decimal`.
//! So a decision never depends on an approximate root (invariant 1), and a
//! close exactly on the band is not below it.
//!
//! It bets, like `rsi_reversion`, that a move unusually far below the
//! recent mean snaps back; the band widens with volatility, so "unusually
//! far" scales with how much the coin has been moving. It fails the same
//! way: in a downtrend each close below the band is bought, and the mean it
//! waits for keeps falling with the price, so it can hold a long way down
//! (no stop). The exit at the mean takes a smaller, more frequent win than
//! waiting for the upper band would. Each round trip pays two fees and two
//! slippages. Expected edge is unverified.
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "bollinger_reversion";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BollingerReversionParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Closes in the mean and the deviation, current included (`20`). At
    /// least 2; the engine's `history_len` must be at least this.
    period: usize,
    /// Standard deviations below the mean that the close must be to buy
    /// (`"2"`). Positive.
    #[serde(deserialize_with = "exact_decimal")]
    k: Decimal,
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
pub struct BollingerReversion {
    p: BollingerReversionParams,
}

/// The window's mean and population variance.
struct Band {
    mid: Decimal,
    var: Decimal,
}

impl BollingerReversion {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: BollingerReversionParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.period < 2 {
            return bad("period must be >= 2");
        }
        if p.k <= Decimal::ZERO {
            return bad("k must be positive");
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

    /// `None` until `period` closes exist.
    fn band(&self, ctx: &Context) -> Option<Band> {
        let n = self.p.period;
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n {
            return None;
        }
        let closes: Vec<Decimal> = hist.rev().take(n).map(|c| c.close.get()).collect();
        let count = Decimal::from(n);
        let mid = closes.iter().sum::<Decimal>() / count;
        let var = closes
            .iter()
            .map(|c| (*c - mid) * (*c - mid))
            .sum::<Decimal>()
            / count;
        Some(Band { mid, var })
    }

    /// close < mid − k·sd, decided without a square root.
    fn below_lower(&self, b: &Band, close: Decimal) -> bool {
        let gap = b.mid - close;
        gap > Decimal::ZERO && gap * gap > self.p.k * self.p.k * b.var
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
    StrategyRegistration { name: NAME, build: BollingerReversion::build }
}

impl Strategy for BollingerReversion {
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
        let Some(b) = self.band(ctx) else {
            return Vec::new();
        };
        let close = c.close.get();
        match self.long_units(ctx, c) {
            None if self.below_lower(&b, close) => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if close > b.mid => vec![Signal::Submit {
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

    fn params_with(period: usize, k: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               period = {period}
               k = "{k}"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params_with(5, "2"), 20)
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
        assert_eq!(build(NAME, &params_with(5, "2")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with(1, "2")).is_err());
        assert!(build(NAME, &params_with(5, "0")).is_err());
        let float_k = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               period = 5
               k = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &float_k).is_err());
    }

    /// Closes 100 x4, 90; period 5: mid 98, deviations 2, 2, 2, 2, −8,
    /// population variance (4·4 + 64) / 5 = 16, sd 4. The lower band is
    /// 98 − 4k: k 1.5 → 92 (90 below: buy); k 2 → exactly 90 (on the band,
    /// not below: nothing); k 1.99 → 90.04 (buy).
    #[test]
    fn buys_below_the_lower_band_and_not_on_it() {
        for (k, buys) in [("1.5", true), ("2", false), ("1.99", true)] {
            let mut s = build(NAME, &params_with(5, k)).unwrap();
            let mut ctx = ctx();
            for i in 0..4 {
                assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
            }
            let want = if buys { vec![buy()] } else { vec![] };
            assert_eq!(bar(s.as_mut(), &mut ctx, 4, "90"), want, "k {k}");
        }
    }

    /// Bought at 90 (k 1.5). Next close 97: window 100, 100, 100, 90, 97,
    /// mid 97.4: still under, hold. Then 99: window 100, 100, 90, 97, 99,
    /// mid 97.2: above → sell.
    #[test]
    fn sells_back_above_the_mean() {
        let mut s = build(NAME, &params_with(5, "1.5")).unwrap();
        let mut ctx = ctx();
        for i in 0..4 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 4, "90"), vec![buy()]);
        hold(&mut ctx, dec!(11.1));
        assert!(bar(s.as_mut(), &mut ctx, 5, "97").is_empty());
        assert_eq!(bar(s.as_mut(), &mut ctx, 6, "99"), vec![sell(dec!(11.1))]);
    }

    /// No spread, no band: a flat window never buys.
    #[test]
    fn flat_prices_never_buy() {
        let mut s = build(NAME, &params_with(5, "1")).unwrap();
        let mut ctx = ctx();
        for i in 0..8 {
            assert!(bar(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
    }

    #[test]
    fn needs_period_closes() {
        let mut s = build(NAME, &params_with(5, "0.5")).unwrap();
        let mut ctx = ctx();
        for (i, p) in [(0, "100"), (1, "100"), (2, "100"), (3, "50")] {
            assert!(bar(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params_with(5, "1.5")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        for i in 0..4 {
            bar(s.as_mut(), &mut ctx, i, "100");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 4, "90"), vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params_with(5, "1.5")).unwrap();
        let mut ctx = ctx();
        for i in 0..4 {
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
        assert!(bar(s.as_mut(), &mut ctx, 4, "90").is_empty());
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with(5, "2")).unwrap();
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
