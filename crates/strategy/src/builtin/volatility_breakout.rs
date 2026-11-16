//! `volatility_breakout`: Larry Williams' volatility breakout, one coin,
//! held for one bar.
//!
//! On each closed bar while flat: place a buy stop at close + `k`·(high −
//! low) of that bar. If the next bar trades up through it, the stop fills
//! during that bar; at its close the whole holding is sold at market (fills
//! at the following open). A stop that didn't trigger is cancelled at the
//! close and replaced by one from the new bar's range. Williams sets the
//! level from the day's open; a 24/7 market's next open is the close, and
//! the close is what is known when the order is placed (invariant 2).
//!
//! Why a stop order rather than a decision on the close: the bet is that a
//! move larger than a fraction of the last bar's range keeps going for the
//! rest of the bar. Waiting for the close to confirm it would buy after the
//! move it means to catch. The stop enters at the moment the threshold
//! trades, as the venue would.
//!
//! What it costs: every trade is a round trip of two taker fees and two
//! slippages, paid on a one-bar holding, so the average continuation after
//! the trigger must beat roughly 2·(fee + slippage). The backtest fills a
//! stop at the stop plus `slippage_bps` (at the open if the bar gaps over
//! it); a live stop-market at a fast breakout can slip more. An entry sized at
//! the stop that fills on a gap spends a little over `quote_amount`.
//! Expected edge is unverified.
//!
//! Cancel and replace go out on the same close, in that order. Live, the
//! old stop could trigger in the moment before its cancel lands, so the
//! strategy can briefly hold two entries; the next close sells everything.
//! While the venue hasn't confirmed a cancel, nothing new is placed. A
//! refused stop (live: price already above it) is simply not retried until
//! the next close. Stateless: the decision is the books plus the last bar,
//! so a restart (whose reconcile cancels the working stop) resumes on the
//! next close, missing at most that one bar's entry.
//!
//! Reads only closed candles through `Context` (invariants 2, 5), in
//! `Decimal` (invariant 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, InstrumentId, Notional, OrderKind, Price, Qty, Side, Signal, Size,
};
use serde::Deserialize;

const NAME: &str = "volatility_breakout";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolatilityBreakoutParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Fraction of the last bar's range above its close where the buy stop
    /// sits (`"0.5"`, Williams' usual value). Positive: at 0 the stop would
    /// sit at the market and trigger at once.
    #[serde(deserialize_with = "exact_decimal")]
    k: Decimal,
    /// Quote spent per entry (`"1000"` USDT), converted to base units at the
    /// stop price.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required, like the other
    /// strategies', so the config states the costs the backtest runs with;
    /// decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per taker fill assumed, in basis points (`"5"`). Required
    /// and unused in decisions, as `fee`; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct VolatilityBreakout {
    p: VolatilityBreakoutParams,
}

impl VolatilityBreakout {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: VolatilityBreakoutParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
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
    StrategyRegistration { name: NAME, build: VolatilityBreakout::build }
}

impl Strategy for VolatilityBreakout {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from the books and the last bar alone, both rebuilt on
    /// a restart: only the format version is saved.
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
        // A cancel the venue hasn't confirmed may still fill: deciding now
        // could stack a second entry on it.
        if c.instrument != *coin || ctx.cancelling_orders(coin).next().is_some() {
            return Vec::new();
        }
        let working: Vec<_> = ctx.open_orders(coin).collect();
        if working.iter().any(|o| o.side == Side::Sell) {
            return Vec::new(); // the exit is still going out
        }
        // Whatever is left working is last bar's buy stop: stale either way.
        let mut out: Vec<Signal> = working
            .iter()
            .map(|o| Signal::Cancel(o.id.clone()))
            .collect();
        if let Some(units) = self.long_units(ctx, c) {
            out.push(Signal::Submit {
                instrument: coin.clone(),
                side: Side::Sell,
                size: Size::Base(units),
                kind: OrderKind::Market,
            });
            return out;
        }
        let range = c.high.get() - c.low.get();
        if range.is_zero() {
            return out; // no volatility: a stop at the close fires at once
        }
        out.push(Signal::Submit {
            instrument: coin.clone(),
            side: Side::Buy,
            size: Size::Quote(self.p.quote_amount),
            kind: OrderKind::StopMarket {
                stop: Price::new(c.close.get() + self.p.k * range),
            },
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::btcusdt;
    use crate::{build, driver, names};
    use bot_core::{CandleSpec, Interval, Order, OrderId, OrderSpec, Position, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(k: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               k = "{k}"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params_with("0.5"), 10)
    }

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

    fn stop_buy(stop: Decimal) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::StopMarket {
                stop: Price::new(stop),
            },
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

    fn order(id: &str, side: Side, kind: OrderKind) -> Order {
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            // Risk turned the stop's quote budget into base units.
            size: Size::Base(Qty::new(dec!(9))),
            kind,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn stale_stop() -> Order {
        order(
            "o-1",
            Side::Buy,
            OrderKind::StopMarket {
                stop: Price::new(dec!(102)),
            },
        )
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params_with("0.5")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with("0")).is_err());
        assert!(build(NAME, &params_with("-0.5")).is_err());
        let float_k = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               k = 0.5
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &float_k).is_err());
    }

    /// One bar is all it needs. Range 104 - 98 = 6: k 0.5 → stop at
    /// 101 + 3 = 104; k 0.25 → 101 + 1.5 = 102.5. Not rounded here: risk
    /// rounds a buy stop up to the tick.
    #[test]
    fn places_a_stop_k_ranges_above_the_close() {
        for (k, stop) in [("0.5", dec!(104)), ("0.25", dec!(102.5))] {
            let mut s = build(NAME, &params_with(k)).unwrap();
            let mut ctx = ctx();
            let got = feed(s.as_mut(), &mut ctx, ohlc(0, "100", "104", "98", "101"));
            assert_eq!(got, vec![stop_buy(stop)], "k {k}");
        }
    }

    /// Last bar's stop, still working, is cancelled before the new one goes
    /// out, in that order (the engine routes in order).
    #[test]
    fn replaces_last_bars_stop() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        driver::set_open_orders(&mut ctx, vec![stale_stop()]);
        let got = feed(s.as_mut(), &mut ctx, ohlc(1, "100", "101", "99", "100"));
        assert_eq!(
            got,
            vec![
                Signal::Cancel(OrderId::new("o-1").unwrap()),
                stop_buy(dec!(101))
            ]
        );
    }

    /// The stop filled during the bar: sell it all at the close.
    #[test]
    fn sells_on_the_close_of_the_entry_bar() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(9.99));
        let got = feed(s.as_mut(), &mut ctx, ohlc(2, "100", "104", "99", "103"));
        assert_eq!(got, vec![sell(dec!(9.99))]);
    }

    /// Both entries filled (live race on a cancel): one sell for the lot,
    /// and any stop still working cancelled with it.
    #[test]
    fn long_with_a_stop_working_cancels_it_and_sells() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(19.98));
        driver::set_open_orders(&mut ctx, vec![stale_stop()]);
        let got = feed(s.as_mut(), &mut ctx, ohlc(2, "100", "104", "99", "103"));
        assert_eq!(
            got,
            vec![
                Signal::Cancel(OrderId::new("o-1").unwrap()),
                sell(dec!(19.98))
            ]
        );
    }

    #[test]
    fn waits_for_its_exit_and_for_unconfirmed_cancels() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(9.99));
        driver::set_open_orders(&mut ctx, vec![order("o-2", Side::Sell, OrderKind::Market)]);
        assert!(feed(s.as_mut(), &mut ctx, ohlc(3, "104", "105", "102", "103")).is_empty());

        let mut ctx = self::ctx();
        driver::set_cancelling_orders(&mut ctx, vec![stale_stop()]);
        assert!(feed(s.as_mut(), &mut ctx, ohlc(3, "104", "105", "102", "103")).is_empty());
    }

    /// A bar with no range would put the stop at the close, where a venue
    /// refuses it (or fills it at once): nothing new, the stale one still
    /// goes.
    #[test]
    fn a_rangeless_bar_places_nothing() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        driver::set_open_orders(&mut ctx, vec![stale_stop()]);
        let got = feed(s.as_mut(), &mut ctx, ohlc(1, "100", "100", "100", "100"));
        assert_eq!(got, vec![Signal::Cancel(OrderId::new("o-1").unwrap())]);
    }

    #[test]
    fn dust_counts_as_cash() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00000099));
        let got = feed(s.as_mut(), &mut ctx, ohlc(0, "100", "104", "98", "101"));
        assert_eq!(got, vec![stop_buy(dec!(104))]);
    }

    #[test]
    fn ignores_other_instruments() {
        let p = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               k = "0.5"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &p).unwrap();
        let mut ctx = ctx();
        assert!(feed(s.as_mut(), &mut ctx, ohlc(0, "100", "104", "98", "101")).is_empty());
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with("0.5")).unwrap();
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
