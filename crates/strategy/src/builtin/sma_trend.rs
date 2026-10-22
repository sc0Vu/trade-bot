//! `sma_trend`: a single-coin moving-average trend filter, long or flat.
//!
//! Holds the coin while its close is above the simple moving average of
//! its last `sma_period` closes, and sits in cash (the quote asset)
//! otherwise. Cash is the defensive position: v1 spot can't short, so a
//! downtrend is sat out, not traded. `band_pct` puts a dead zone around the
//! average, so a close wobbling across it doesn't flip the position every
//! bar.
//!
//! The average includes the current, closed bar and nothing later
//! (invariant 2); positions and history come only from `Context`
//! (invariant 5); all arithmetic is `Decimal` (invariant 1). Expected edge
//! is unverified: a trend filter pays a round trip (two fees, slippage on
//! both market legs) per whipsaw, and only wins when trends run long
//! enough to cover that.

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "sma_trend";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SmaTrendParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Closes in the average, current bar included. The engine's
    /// `history_len` must be at least this, or the average never fills and
    /// the strategy never trades.
    sma_period: usize,
    /// Quote spent each time the strategy goes long (`"1000"` USDT), as a
    /// quote-budget market buy: never more than this is spent.
    quote_amount: Notional,
    /// Dead zone around the average, as a fraction (`"0.01"` = 1%): go long
    /// only above `sma * (1 + band_pct)`, go to cash only below
    /// `sma * (1 - band_pct)`. `"0"` (the default) means none.
    #[serde(deserialize_with = "exact_decimal", default)]
    band_pct: Decimal,
    /// Fee per trade assumed (`"0.001"`). Required, like `ratio_hop`'s, so
    /// the config states the costs the backtest is run with; v1 decisions
    /// don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused in decisions, as `fee`; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs. Long or flat on one coin, decided on each of its
/// closed candles.
pub struct SmaTrend {
    p: SmaTrendParams,
}

impl SmaTrend {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: SmaTrendParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.sma_period == 0 {
            return bad("sma_period must be >= 1");
        }
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
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

    /// Mean of the last `sma_period` closes, ending at the current bar;
    /// `None` until that many have closed.
    fn sma(&self, ctx: &Context) -> Option<Decimal> {
        let n = self.p.sma_period;
        let hist = ctx.history(&self.p.coin);
        if hist.len() < n {
            return None;
        }
        let sum: Decimal = hist.rev().take(n).map(|c| c.close.get()).sum();
        Some(sum / Decimal::from(n))
    }

    /// Lot-rounded units held, if a sell of them would pass the venue's
    /// filters at `c`'s close. `None` is cash: flat, or dust a buy's base fee
    /// left that can't be sold. Treating dust as long would leave the
    /// strategy unable to sell it and unwilling to buy again.
    fn long_units(&self, ctx: &Context, c: &Candle) -> Option<bot_core::Qty> {
        let inst = ctx.instrument(&self.p.coin)?;
        let units = inst.round_qty(ctx.position(&self.p.coin).qty());
        inst.check_order(units, c.close).ok().map(|()| units)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: SmaTrend::build }
}

impl Strategy for SmaTrend {
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
        // An order still working (a market order not yet filled live, or
        // one the venue is slow to close) would be doubled by deciding
        // again; wait for it.
        if c.instrument != *coin || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        let Some(sma) = self.sma(ctx) else {
            return Vec::new();
        };
        let close = c.close.get();
        let band = self.p.band_pct;
        match self.long_units(ctx, c) {
            None if close > sma * (Decimal::ONE + band) => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if close < sma * (Decimal::ONE - band) => vec![Signal::Submit {
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
    use bot_core::{Order, OrderId, OrderSpec, Position, Qty, Timestamp};
    use rust_decimal_macros::dec;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               sma_period = 3
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(""), 10)
    }

    /// Feeds candle `i` at `price` as the engine does; returns the signals.
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
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        for extra in [
            "band_pct = \"-0.01\"",
            "band_pct = \"1\"",
            // A TOML number would pass through a float first (invariant 1).
            "band_pct = 0.01",
        ] {
            assert!(build(NAME, &params(extra)).is_err(), "{extra}");
        }
        let zero_period = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               sma_period = 0
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &zero_period).is_err());
        let no_slippage = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               sma_period = 3
               quote_amount = "1000"
               fee = "0.001""#,
        )
        .unwrap();
        assert!(
            build(NAME, &no_slippage).is_err(),
            "slippage_bps is required"
        );
    }

    /// sma_period 3: bars 0 and 1 can't form an average, even with the
    /// close far above everything before it.
    #[test]
    fn not_enough_history_yet_means_no_signal() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        assert!(bar(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "500").is_empty());
        // Bar 2 completes the window: (100 + 500 + 600) / 3 = 400 < 600.
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "600"), vec![buy()]);
    }

    /// Closes 100, 100, 100, 110: the SMA at bar 3 is 310/3 = 103.33…, so
    /// 110 crosses above it. Held, closes 112 then 105: SMA (110 + 112 +
    /// 105)/3 = 109 > 105, so bar 5 sells the whole holding.
    #[test]
    fn buys_on_the_cross_up_and_sells_on_the_cross_down() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        for (i, p) in [(0, "100"), (1, "100"), (2, "100")] {
            assert!(bar(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "110"), vec![buy()]);
        hold(&mut ctx, dec!(8.991));
        assert!(
            bar(s.as_mut(), &mut ctx, 4, "112").is_empty(),
            "still above"
        );
        assert_eq!(bar(s.as_mut(), &mut ctx, 5, "105"), vec![sell(dec!(8.991))]);
    }

    /// With a 5% band the bar-5 close of 105 is above 109 * 0.95 = 103.55:
    /// inside the dead zone, so no sell. The bar-3 close of 110 still
    /// clears 103.33… * 1.05 = 108.5, so the buy fires.
    #[test]
    fn band_suppresses_a_shallow_cross() {
        let mut s = build(NAME, &params(r#"band_pct = "0.05""#)).unwrap();
        let mut ctx = ctx();
        for (i, p) in [(0, "100"), (1, "100"), (2, "100")] {
            bar(s.as_mut(), &mut ctx, i, p);
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "110"), vec![buy()]);
        hold(&mut ctx, dec!(8.991));
        bar(s.as_mut(), &mut ctx, 4, "112");
        assert!(bar(s.as_mut(), &mut ctx, 5, "105").is_empty());
    }

    /// 0.00000099 BTC left after a sell rounds to 0 lots: that is cash, so
    /// the next cross up buys again instead of trying to sell dust.
    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        for (i, p) in [(0, "100"), (1, "100"), (2, "100")] {
            bar(s.as_mut(), &mut ctx, i, p);
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 3, "110"), vec![buy()]);
    }

    /// Long and below the average, but 0.00001 BTC at 90 is 0.0009 USDT,
    /// under the 5 USDT minimum: not sellable, so no doomed sell.
    #[test]
    fn unsellable_holding_emits_no_sell() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00001));
        for (i, p) in [(0, "100"), (1, "100"), (2, "90")] {
            assert!(bar(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        for (i, p) in [(0, "100"), (1, "100"), (2, "100")] {
            bar(s.as_mut(), &mut ctx, i, p);
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
        assert!(bar(s.as_mut(), &mut ctx, 3, "110").is_empty());
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               sma_period = 1
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &p).unwrap();
        let mut ctx = ctx();
        // BTC candles only; sma_period 1 would otherwise decide at once.
        for (i, price) in [(0, "100"), (1, "200")] {
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
