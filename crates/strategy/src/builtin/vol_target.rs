//! `vol_target`: hold one coin at a weight that keeps the portfolio's
//! volatility near a target: more when the coin is calm, less when it
//! swings.
//!
//! Each closed bar: `vol` = [`indicator::return_sd`] over `vol_period`
//! returns, annualized by √(bars per 365-day year) of the candle's
//! interval. Target weight = `target_vol / vol`, capped at `max_weight`
//! (at most 1: spot can't borrow); no movement at all → `max_weight`. The
//! position is then traded to that weight of `capital` with market orders
//! whenever it is more than `band` away, exactly as `rebalance` does with a
//! fixed weight (shared `portfolio` code: own cash derived from the books,
//! sells' proceeds fund buys, filter-failing trades dropped).
//!
//! Why it can help: volatility clusters, so a stretch of large swings tends
//! to be followed by more. Cutting exposure while swings are wide cuts
//! most of the drawdown that comes with them (crashes arrive in high
//! volatility); returns aren't predicted at all. What it costs: it trades
//! whenever volatility moves the weight past the band, paying fee and
//! slippage on the traded amount; it is late at turning points (vol is
//! measured on past bars), so after a crash it buys back in only once
//! swings calm; and it holds less of the coin in volatile rallies.
//! Expected edge is unverified.
//!
//! The volatility uses `Decimal`'s approximate square root (close, not
//! exact): it only sets a weight. Orders and cash stay exact (invariant 1).
//! Stateless: positions and history are all a decision needs, both rebuilt
//! on a restart. Reads only closed candles through `Context` (invariants 2,
//! 5).

use super::portfolio::{self, Backoff, Target};
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Interval, Notional, Signal};
use rust_decimal::MathematicalOps;
use serde::Deserialize;

const NAME: &str = "vol_target";

fn one() -> Decimal {
    Decimal::ONE
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolTargetParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
    /// Annualized volatility aimed for (`"0.4"` = 40% a year). BTC has
    /// mostly run at 40-80%; a target below the coin's own volatility
    /// holds less than all of `capital` most of the time.
    #[serde(deserialize_with = "exact_decimal")]
    target_vol: Decimal,
    /// Returns the volatility is measured over. Needs `vol_period + 1`
    /// closed candles, so the engine's `history_len` must be at least that.
    /// At least 2.
    vol_period: usize,
    /// Most of `capital` ever held in the coin (`"1"`, the default). In
    /// (0, 1]: spot can't hold more than it pays for.
    #[serde(deserialize_with = "exact_decimal", default = "one")]
    max_weight: Decimal,
    /// How far the weight may stray from its target before trading back,
    /// in absolute fraction (`"0.05"` = 5 points). In (0, 1).
    #[serde(deserialize_with = "exact_decimal")]
    band: Decimal,
    /// Fee per trade assumed (`"0.001"`): with slippage, discounts the
    /// sells' proceeds the buys may spend.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`); match
    /// the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct VolTarget {
    p: VolTargetParams,
    backoff: Backoff,
}

impl VolTarget {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: VolTargetParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !p.capital.is_positive() {
            return bad("capital must be positive");
        }
        if p.target_vol <= Decimal::ZERO {
            return bad("target_vol must be positive");
        }
        if p.vol_period < 2 {
            return bad("vol_period must be >= 2");
        }
        if p.max_weight <= Decimal::ZERO || p.max_weight > Decimal::ONE {
            return bad("max_weight must be in (0, 1]");
        }
        if p.band <= Decimal::ZERO || p.band >= Decimal::ONE {
            return bad("band must be in (0, 1)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self {
            p,
            backoff: Backoff::default(),
        }))
    }

    /// Target weight on the current bar; `None` until `vol_period + 1`
    /// candles have closed.
    fn weight(&self, ctx: &Context, interval: Interval) -> Option<Decimal> {
        let sd = indicator::return_sd(ctx.history(&self.p.coin), self.p.vol_period)?;
        let per_year = Decimal::from(365 * 86_400_000_i64) / Decimal::from(interval.as_millis());
        let vol = sd * per_year.sqrt()?;
        if vol.is_zero() {
            return Some(self.p.max_weight);
        }
        Some((self.p.target_vol / vol).min(self.p.max_weight))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: VolTarget::build }
}

impl Strategy for VolTarget {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from positions and history alone, both rebuilt on a
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

    /// A fill means the venue is taking orders: stop backing off.
    fn on_fill(&mut self, _ctx: &mut Context, _f: &Fill) {
        self.backoff.filled();
    }

    /// Refused (e.g. `-2010`: `capital` more than the account holds): wait
    /// longer before asking again (`portfolio::Backoff`).
    fn on_rejected(&mut self, ctx: &mut Context, _s: &Signal, reason: &str) {
        tracing::warn!(strategy = NAME, reason, "order refused; backing off");
        self.backoff.refused(ctx.now());
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        if c.instrument != *coin
            || ctx.open_orders(coin).next().is_some()
            || ctx.cancelling_orders(coin).next().is_some()
        {
            return Vec::new();
        }
        let Some(weight) = self.weight(ctx, c.interval) else {
            return Vec::new();
        };
        if self.backoff.hold() {
            return Vec::new();
        }
        let target = [Target {
            coin: coin.clone(),
            weight,
        }];
        let cost = self.p.fee + self.p.slippage_bps / Decimal::from(10_000);
        portfolio::rebalance(ctx, &target, self.p.capital, self.p.band, cost).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::portfolio::tests::position;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{Order, OrderId, OrderKind, OrderSpec, Qty, Side, Size, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               capital = "2000"
               vol_period = 3
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn params() -> Params {
        params_with(r#"target_vol = "0.8""#)
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(), 10)
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    fn weight_after(extra: &str, prices: &[&str]) -> Option<Decimal> {
        let s = VolTarget {
            p: params_with(extra).parse(NAME).unwrap(),
            backoff: Backoff::default(),
        };
        let mut ctx = ctx();
        for (i, p) in prices.iter().enumerate() {
            driver::push_candle(&mut ctx, candle(i as i64, p));
        }
        s.weight(&ctx, Interval::H1)
    }

    fn buy(budget: Decimal) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(budget)),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        let bad = |extra: &str| build(NAME, &params_with(extra)).is_err();
        assert!(bad(r#"target_vol = "0""#));
        assert!(bad(r#"target_vol = "0.8"
                       max_weight = "1.5""#));
        assert!(bad(r#"target_vol = "0.8"
                       max_weight = "0""#));
        assert!(bad("target_vol = 0.8"), "float");
        let p = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               capital = "2000"
               target_vol = "0.8"
               vol_period = 1
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &p).is_err(), "vol_period 1");
    }

    /// Hourly closes 50, 100, 110, 99, 99: returns +10%, −10%, 0 → per-bar
    /// sd 0.1 (the indicator's test). Annualized 0.1·√8760 = 9.3594…;
    /// target 4.68 → weight 4.68 / 9.3594 = 0.50003 (the root is
    /// approximate: compared to 4 dp). A target above the vol caps at
    /// max_weight; no movement → max_weight.
    #[test]
    fn weight_is_target_over_annualized_vol_capped() {
        let swings = ["50", "100", "110", "99", "99"];
        let w = weight_after(r#"target_vol = "4.68""#, &swings).unwrap();
        assert_eq!(w.round_dp(4), dec!(0.5000));
        let w = weight_after(r#"target_vol = "20""#, &swings).unwrap();
        assert_eq!(w, dec!(1), "default max_weight");
        let w = weight_after(
            r#"target_vol = "20"
               max_weight = "0.7""#,
            &swings,
        );
        assert_eq!(w, Some(dec!(0.7)));
        let w = weight_after(r#"target_vol = "0.8""#, &["100", "100", "100", "100"]);
        assert_eq!(w, Some(dec!(1)));
        assert_eq!(
            weight_after(r#"target_vol = "0.8""#, &["100", "110", "99"]),
            None
        );
    }

    /// Calm start: weight max (1) of 2000 → buy 2000 once 4 closes exist.
    /// Then holding 20 BTC at 100 (basis 2000, cash 0), swings of ±10%
    /// push vol to 9.36/yr: weight 0.8 / 9.36 = 0.085, far out of band →
    /// sell most of it.
    #[test]
    fn buys_when_calm_and_sells_when_swings_widen() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        for i in 0..3 {
            assert!(feed(s.as_mut(), &mut ctx, i, "100").is_empty(), "bar {i}");
        }
        assert_eq!(feed(s.as_mut(), &mut ctx, 3, "100"), vec![buy(dec!(2000))]);

        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = self::ctx();
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(20), dec!(2000), dec!(0)),
        );
        for (i, p) in [(0, "50"), (1, "100"), (2, "110"), (3, "99")] {
            feed(s.as_mut(), &mut ctx, i, p);
        }
        let got = feed(s.as_mut(), &mut ctx, 4, "99");
        let [
            Signal::Submit {
                side: Side::Sell,
                size: Size::Base(q),
                ..
            },
        ] = got.as_slice()
        else {
            panic!("expected one sell, got {got:?}");
        };
        // Value 1980, weight ≈ 0.0855 → keep ≈ 169.2 of BTC, sell ≈ 18.29.
        assert!(q.get() > dec!(18.2) && q.get() < dec!(18.4), "{q:?}");
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        for i in 0..4 {
            assert!(feed(s.as_mut(), &mut ctx, i, "100").is_empty());
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
