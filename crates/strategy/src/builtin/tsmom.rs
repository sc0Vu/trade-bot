//! `tsmom`: time-series momentum with volatility targeting, after
//! Moskowitz, Ooi and Pedersen (2012), long-only.
//!
//! Each close time, once every coin's candle is in and nothing is working,
//! each coin is judged on its own (not against the others): long if its
//! return over the last `lookback` bars, `close / close[lookback bars ago]
//! − 1`, is above zero, else out. A long coin's weight is
//! min(`max_weight`, `target_vol` / its annualized volatility) / N, where
//! the volatility is [`indicator::return_sd`] over `vol_period` returns ×
//! √(bars per 365-day year) and N is the number of coins, so the weights
//! never sum past 1 (spot can't borrow). No movement at all → `max_weight`.
//! Holdings are traded to those weights of `capital` with market orders
//! once any is more than `band` off (the shared `portfolio` code: own cash
//! from the books, sells first, their proceeds fund the buys).
//!
//! Why it may work: trends in an asset's own past return have persisted in
//! futures markets for a century of data, and the trend filters here cut
//! drawdown in our crypto backtests; scaling to volatility cuts it again in
//! the stretches that cause most of it (volatility clusters). How it fails:
//! choppy markets (each false signal is a round trip of fees and
//! slippage), turning points (the signal lags by up to `lookback` bars),
//! and the weight moves with volatility, so it trades even inside a trend.
//! A return of exactly zero is out. Long-only: the published strategy also
//! shorts; v1 spot can't. Expected edge is unverified.
//!
//! The volatility uses `Decimal`'s approximate square root: it sets
//! weights only; orders and cash stay exact (invariant 1). Stateless:
//! positions and history are all a decision needs, both rebuilt on a
//! restart. Reads only closed candles through `Context` (invariants 2, 5).
//! All coins must share one quote asset, else it never trades (logged).

use super::portfolio::{self, Backoff, Target};
use super::rebalance::{completes, one_quote};
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Interval, Notional, Signal};
use rust_decimal::MathematicalOps;
use serde::Deserialize;

const NAME: &str = "tsmom";

fn one() -> Decimal {
    Decimal::ONE
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TsmomParams {
    /// The coins traded (`["binance:BTCUSDT"]`), one quote asset. Each is
    /// judged on its own trend.
    coins: Vec<InstrumentId>,
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
    /// Bars the trend is measured over (`30` daily bars ≈ a month). At
    /// least 1.
    lookback: usize,
    /// Annualized volatility aimed for per coin (`"0.4"` = 40% a year),
    /// before the 1/N split.
    #[serde(deserialize_with = "exact_decimal")]
    target_vol: Decimal,
    /// Returns the volatility is measured over; at least 2. The engine's
    /// `history_len` must be at least `max(lookback, vol_period) + 1`.
    vol_period: usize,
    /// Most of a coin's 1/N share held (`"1"`, the default), in (0, 1].
    #[serde(deserialize_with = "exact_decimal", default = "one")]
    max_weight: Decimal,
    /// How far a weight may drift before everything is traded back, in
    /// absolute fraction (`"0.05"` = 5 points). In (0, 1).
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
pub struct Tsmom {
    p: TsmomParams,
    stopped: bool,
    backoff: Backoff,
}

impl Tsmom {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: TsmomParams = params.parse(NAME)?;
        let bad = |reason: String| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason,
            })
        };
        if let Err(e) = portfolio::check_coins(&p.coins, 1) {
            return bad(e);
        }
        if p.lookback == 0 {
            return bad("lookback must be >= 1".into());
        }
        if p.target_vol <= Decimal::ZERO {
            return bad("target_vol must be positive".into());
        }
        if p.vol_period < 2 {
            return bad("vol_period must be >= 2".into());
        }
        if p.max_weight <= Decimal::ZERO || p.max_weight > Decimal::ONE {
            return bad("max_weight must be in (0, 1]".into());
        }
        if let Err(e) = portfolio::check_common(p.capital, p.band, p.fee, p.slippage_bps) {
            return bad(e.into());
        }
        Ok(Box::new(Self {
            p,
            stopped: false,
            backoff: Backoff::default(),
        }))
    }

    /// One coin's weight: its volatility-scaled share if its trend is up,
    /// else 0. `None` until it has `max(lookback, vol_period) + 1` closes.
    fn weight(&self, ctx: &Context, coin: &InstrumentId, interval: Interval) -> Option<Decimal> {
        let need = self.p.lookback.max(self.p.vol_period) + 1;
        let hist = ctx.history(coin);
        if hist.len() < need {
            return None;
        }
        let closes: Vec<Decimal> = hist
            .rev()
            .take(self.p.lookback + 1)
            .map(|c| c.close.get())
            .collect();
        let (now, then) = (closes[0], closes[self.p.lookback]);
        if now <= then {
            return Some(Decimal::ZERO);
        }
        let sd = indicator::return_sd(ctx.history(coin), self.p.vol_period)?;
        let per_year = Decimal::from(365 * 86_400_000_i64) / Decimal::from(interval.as_millis());
        let vol = sd * per_year.sqrt()?;
        let w = if vol.is_zero() {
            self.p.max_weight
        } else {
            (self.p.target_vol / vol).min(self.p.max_weight)
        };
        Some(w / Decimal::from(self.p.coins.len()))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Tsmom::build }
}

impl Strategy for Tsmom {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, &self.p.coins);
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: tsmom stopped"
            );
        }
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
        let coins = &self.p.coins;
        if self.stopped
            || !coins.contains(&c.instrument)
            || !completes(ctx, c, coins.iter())
            || !portfolio::idle(ctx, coins)
        {
            return Vec::new();
        }
        let Some(targets) = coins
            .iter()
            .map(|coin| {
                Some(Target {
                    coin: coin.clone(),
                    weight: self.weight(ctx, coin, c.interval)?,
                })
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Vec::new();
        };
        if self.backoff.hold() {
            return Vec::new();
        }
        let cost = portfolio::trade_cost(self.p.fee, self.p.slippage_bps);
        portfolio::rebalance(ctx, &targets, self.p.capital, self.p.band, cost).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::portfolio::tests::{eth_candle, ethusdt, position};
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{OrderKind, Qty, Side, Size};
    use rust_decimal_macros::dec;

    fn params_with(coins: &str, extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = {coins}
               capital = "2000"
               lookback = 2
               vol_period = 2
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    const BTC: &str = r#"["binance:BTCUSDT"]"#;
    const BOTH: &str = r#"["binance:BTCUSDT", "binance:ETHUSDT"]"#;

    fn strategy(coins: &str, extra: &str) -> Tsmom {
        Tsmom {
            p: params_with(coins, extra).parse(NAME).unwrap(),
            stopped: false,
            backoff: Backoff::default(),
        }
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), ethusdt()], Params::default(), 10)
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(
            build(NAME, &params_with(BTC, r#"target_vol = "1""#))
                .unwrap()
                .name(),
            NAME
        );
    }

    #[test]
    fn rejects_bad_params() {
        let bad = |coins: &str, extra: &str| build(NAME, &params_with(coins, extra)).is_err();
        assert!(bad("[]", r#"target_vol = "1""#));
        assert!(bad(
            r#"["binance:BTCUSDT", "binance:BTCUSDT"]"#,
            r#"target_vol = "1""#
        ));
        assert!(bad(BTC, r#"target_vol = "0""#));
        assert!(bad(BTC, "target_vol = 1.5"), "float");
        assert!(bad(BTC, "target_vol = \"1\"\nmax_weight = \"1.1\""));
        let p = Params::from_toml(
            "coins = [\"binance:BTCUSDT\"]\ncapital = \"2000\"\nlookback = 0\nvol_period = 2\n\
             target_vol = \"1\"\nband = \"0.05\"\nfee = \"0.001\"\nslippage_bps = \"10\"",
        )
        .unwrap();
        assert!(build(NAME, &p).is_err(), "lookback 0");
    }

    /// Closes 100, 110, 99 (lookback 2: 99 vs 100, down) → out (0).
    /// Closes 100, 110, 121: up; returns +10%, +10% → sd 0 → max_weight,
    /// split over N.
    /// Closes 100, 110, 121.1: up; returns 0.1, 0.1009091 → sd =
    /// 0.0009091/√2 = 0.00064282; annualized ·√8760 = 0.0601650; target
    /// 0.03 → 0.498629; one coin of one → that.
    #[test]
    fn weight_is_trend_gated_and_volatility_scaled() {
        let s = strategy(BTC, r#"target_vol = "0.03""#);
        let btc = btcusdt().id.clone();
        let weight_after = |s: &Tsmom, prices: &[&str]| {
            let mut ctx = ctx();
            for (i, p) in prices.iter().enumerate() {
                driver::push_candle(&mut ctx, candle(i as i64, p));
            }
            s.weight(&ctx, &btc, Interval::H1)
        };
        assert_eq!(weight_after(&s, &["100", "110"]), None, "needs 3 closes");
        assert_eq!(weight_after(&s, &["100", "110", "99"]), Some(dec!(0)));
        assert_eq!(
            weight_after(&s, &["100", "110", "100"]),
            Some(dec!(0)),
            "flat is out"
        );
        assert_eq!(weight_after(&s, &["100", "110", "121"]), Some(dec!(1)));
        let w = weight_after(&s, &["100", "110", "121.1"]).unwrap();
        assert_eq!(w.round_dp(5), dec!(0.49863));
        let two = strategy(BOTH, r#"target_vol = "0.03""#);
        assert_eq!(weight_after(&two, &["100", "110", "121"]), Some(dec!(0.5)));
    }

    /// Up-trend with zero spread: buy all of capital. Then a fall past the
    /// lookback: sell the whole (lot-rounded) holding.
    #[test]
    fn buys_an_uptrend_and_exits_when_it_turns() {
        let mut s = build(NAME, &params_with(BTC, r#"target_vol = "1""#)).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        assert!(feed(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(feed(s.as_mut(), &mut ctx, 1, "110").is_empty());
        assert_eq!(
            feed(s.as_mut(), &mut ctx, 2, "121"),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(2000))),
                kind: OrderKind::Market,
            }]
        );
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(16.5), dec!(2000), dec!(0)),
        );
        // 121 → 110: still above 110 two bars back? 110 vs 110: not up.
        let got = feed(s.as_mut(), &mut ctx, 3, "110");
        assert_eq!(
            got,
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(16.5))),
                kind: OrderKind::Market,
            }]
        );
    }

    /// Two coins: only the one trending up is bought, at half of capital.
    #[test]
    fn each_coin_is_judged_on_its_own() {
        let mut s = build(NAME, &params_with(BOTH, r#"target_vol = "1""#)).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        let mut last = Vec::new();
        for (i, (b, e)) in [("100", "10"), ("110", "9"), ("121", "8.1")]
            .into_iter()
            .enumerate()
        {
            for c in [candle(i as i64, b), eth_candle(i as i64, e)] {
                driver::set_now(&mut ctx, c.close_time);
                driver::push_candle(&mut ctx, c.clone());
                last = s.on_candle(&mut ctx, &c);
            }
        }
        assert_eq!(
            last,
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(1000))),
                kind: OrderKind::Market,
            }]
        );
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with(BTC, r#"target_vol = "1""#)).unwrap();
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
