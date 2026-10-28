//! `dca`: dollar-cost averaging. Spend a fixed quote amount on one coin
//! once per UTC hour, day, week or month, and hold.
//!
//! It makes no prediction. Buying the same amount on a schedule buys more
//! units when the price is low and fewer when it is high, so the average
//! cost is the harmonic mean of the prices paid, never above their simple
//! average. What it removes is the risk of putting everything in at one bad
//! moment, the weakness of [`buy_and_hold`](super::buy_and_hold) started at
//! a top. What it keeps is the coin's direction: in a long decline it buys
//! all the way down and never exits. Each buy pays a taker fee and
//! slippage, so very frequent small buys cost more than rare large ones.
//!
//! Buys on the coin's first candle, then on the first candle closing in
//! each new period (`every`; weeks start Monday). Optional `max_buys` stops
//! it after that many. A buy is skipped while an order on the coin is still
//! working, and tried again on the next candle of the same period; a
//! rejected buy (`-2010`: out of cash) is not retried until the next period
//! and doesn't count. Never sells.
//!
//! Reads only `Context` and returns a `Signal` (invariant 5); the decision
//! is made on a closed candle (invariant 2); money is `Decimal` (invariant
//! 1).

use super::calendar::{Period, period_start};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "dca";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DcaParams {
    /// The one instrument bought (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Quote spent on each buy (`"100"` USDT), as a quote-budget market
    /// order: never more than this per buy.
    quote_amount: Notional,
    /// `"hour"`, `"day"`, `"week"` or `"month"` (UTC). Required: the
    /// schedule is the whole strategy.
    every: Period,
    /// Stop after this many buys. Unset: buy every period until the cash
    /// runs out (then each buy is rejected).
    max_buys: Option<u32>,
    /// Fee per trade assumed (`"0.001"`). Required and unused in decisions,
    /// as in the other single-coin built-ins.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market buy assumed, in basis points (`"5"`). Required
    /// and unused in decisions; match the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct Dca {
    p: DcaParams,
    /// Start (ms) of the last period a buy was sent in.
    last_period: Option<i64>,
    /// Buys sent and not rejected.
    buys: u32,
}

impl Dca {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: DcaParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.max_buys == Some(0) {
            return bad("max_buys must be >= 1");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self {
            p,
            last_period: None,
            buys: 0,
        }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Dca::build }
}

impl Strategy for Dca {
    fn name(&self) -> &str {
        NAME
    }

    /// The period last bought in and the count, so a restart neither buys
    /// twice in one period nor forgets `max_buys`.
    fn save_state(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "v": super::STATE_VERSION,
            "last_period": self.last_period,
            "buys": self.buys,
        }))
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        let last_period = match &state["last_period"] {
            serde_json::Value::Null => None,
            v => Some(
                v.as_i64()
                    .ok_or_else(|| super::state_error(NAME, "bad last_period"))?,
            ),
        };
        let buys = state["buys"]
            .as_u64()
            .and_then(|b| u32::try_from(b).ok())
            .ok_or_else(|| super::state_error(NAME, "missing buys"))?;
        self.last_period = last_period;
        self.buys = buys;
        Ok(())
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        if c.instrument != *coin || self.p.max_buys.is_some_and(|m| self.buys >= m) {
            return Vec::new();
        }
        let period = period_start(self.p.every, c.close_time).as_millis();
        if self.last_period == Some(period) || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        self.last_period = Some(period);
        self.buys += 1;
        vec![Signal::Submit {
            instrument: coin.clone(),
            side: Side::Buy,
            size: Size::Quote(self.p.quote_amount),
            kind: OrderKind::Market,
        }]
    }

    /// A refused buy (out of cash, below the venue minimum) doesn't count
    /// toward `max_buys`; the period stays used, so it isn't resent on
    /// every candle.
    fn on_rejected(&mut self, _ctx: &mut Context, s: &Signal, _reason: &str) {
        if matches!(s, Signal::Submit { instrument, .. } if *instrument == self.p.coin) {
            self.buys = self.buys.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{MarketEvent, Order, OrderId, OrderSpec, Timestamp};
    use rust_decimal_macros::dec;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "100"
               fee = "0.001"
               slippage_bps = "5"
               {extra}"#
        ))
        .unwrap()
    }

    fn buy() -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(100))),
            kind: OrderKind::Market,
        }
    }

    /// Indices of the hourly candles `0..n` (hour i since 1970-01-01, a
    /// Thursday) that buy.
    fn buys_at(s: &mut dyn Strategy, ctx: &mut Context, n: i64) -> Vec<i64> {
        (0..n)
            .filter(|&i| {
                let out = s.on_event(ctx, &MarketEvent::Candle(candle(i, "100")));
                assert!(out.is_empty() || out == vec![buy()], "{out:?}");
                !out.is_empty()
            })
            .collect()
    }

    fn start(extra: &str) -> (Box<dyn Strategy>, Context) {
        let s = build(NAME, &params(extra)).unwrap();
        (s, driver::new(vec![btcusdt()], Params::default(), 10))
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(
            build(NAME, &params(r#"every = "day""#)).unwrap().name(),
            NAME
        );
    }

    #[test]
    fn bad_params_error_at_build() {
        assert!(build(NAME, &params("")).is_err(), "every is required");
        assert!(build(NAME, &params(r#"every = "year""#)).is_err());
        assert!(build(NAME, &params("every = \"day\"\nmax_buys = 0")).is_err());
        let zero = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "0"
               every = "day"
               fee = "0.001"
               slippage_bps = "5""#,
        )
        .unwrap();
        assert!(build(NAME, &zero).is_err());
        let float = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = 100.0
               every = "day"
               fee = "0.001"
               slippage_bps = "5""#,
        )
        .unwrap();
        assert!(build(NAME, &float).is_err(), "money must be a string");
        let no_slippage = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "100"
               every = "day"
               fee = "0.001""#,
        )
        .unwrap();
        assert!(build(NAME, &no_slippage).is_err());
    }

    /// Hour 23's candle closes at 23:59:59.999, still day 0; hour 24's
    /// closes in day 1.
    #[test]
    fn buys_on_the_first_candle_of_each_day() {
        let (mut s, mut ctx) = start(r#"every = "day""#);
        assert_eq!(buys_at(s.as_mut(), &mut ctx, 50), vec![0, 24, 48]);
    }

    /// 1970-01-05 (hour 96) was the first Monday; the next is hour 264.
    #[test]
    fn buys_on_the_first_candle_of_each_week() {
        let (mut s, mut ctx) = start(r#"every = "week""#);
        assert_eq!(buys_at(s.as_mut(), &mut ctx, 300), vec![0, 96, 264]);
    }

    #[test]
    fn stops_after_max_buys() {
        let (mut s, mut ctx) = start("every = \"hour\"\nmax_buys = 3");
        assert_eq!(buys_at(s.as_mut(), &mut ctx, 10), vec![0, 1, 2]);
    }

    /// An order still working holds the buy back to a later candle of the
    /// same period, never into a second buy that period.
    #[test]
    fn waits_for_a_working_order() {
        let (mut s, mut ctx) = start(r#"every = "day""#);
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(100))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        assert!(buys_at(s.as_mut(), &mut ctx, 3).is_empty());
        driver::set_open_orders(&mut ctx, vec![]);
        let later = s.on_event(&mut ctx, &MarketEvent::Candle(candle(3, "100")));
        assert_eq!(later, vec![buy()]);
        let same_day = s.on_event(&mut ctx, &MarketEvent::Candle(candle(4, "100")));
        assert!(same_day.is_empty());
    }

    /// Refused: not resent that period, not counted toward max_buys.
    #[test]
    fn a_rejected_buy_waits_for_the_next_period_and_does_not_count() {
        let (mut s, mut ctx) = start("every = \"hour\"\nmax_buys = 2");
        assert_eq!(buys_at(s.as_mut(), &mut ctx, 1), vec![0]);
        s.on_rejected(&mut ctx, &buy(), "insufficient balance");
        let rest: Vec<i64> = (1..5)
            .filter(|&i| {
                !s.on_event(&mut ctx, &MarketEvent::Candle(candle(i, "100")))
                    .is_empty()
            })
            .collect();
        assert_eq!(rest, vec![1, 2], "two counted buys after the refusal");
    }

    #[test]
    fn state_survives_a_restart() {
        let (mut s, mut ctx) = start("every = \"day\"\nmax_buys = 5");
        assert_eq!(buys_at(s.as_mut(), &mut ctx, 30), vec![0, 24]);
        let state = s.save_state().unwrap();
        assert_eq!(
            state,
            serde_json::json!({ "v": 1, "last_period": 86_400_000, "buys": 2 })
        );
        let (mut fresh, mut ctx) = start("every = \"day\"\nmax_buys = 5");
        fresh.restore_state(state, &ctx).unwrap();
        // Still day 1: no second buy; day 2 buys.
        let next: Vec<i64> = (30..50)
            .filter(|&i| {
                !fresh
                    .on_event(&mut ctx, &MarketEvent::Candle(candle(i, "100")))
                    .is_empty()
            })
            .collect();
        assert_eq!(next, vec![48]);
        let fresh_state = serde_json::json!({ "v": 1, "last_period": null, "buys": 0 });
        assert!(fresh.restore_state(fresh_state, &ctx).is_ok());
        assert!(
            fresh
                .restore_state(serde_json::json!({ "v": 1 }), &ctx)
                .is_err()
        );
    }
}
