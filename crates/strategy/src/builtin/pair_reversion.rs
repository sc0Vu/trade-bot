//! `pair_reversion`: hold whichever of two coins is cheap relative to the
//! other, betting their price ratio returns to its mean.
//!
//! Ratio r = close(A) / close(B), over the last `period` close times
//! (current included). With mean m and population variance v of those
//! ratios: r below m by more than `entry_z` standard deviations → A is
//! cheap → hold A; above by more than that → B is cheap → hold B;
//! otherwise keep what is held. The test is exact, without a square root:
//! (r − m)² > `entry_z`²·v, with the sign of r − m. Before the first
//! signal it holds nothing.
//!
//! Holding a coin means all of `capital` in it: switching sells the one
//! held and buys the other with the sale's estimated proceeds, in one
//! evaluation, through the shared `portfolio` code (own cash from the
//! books, sells first, buys sized to cash + proceeds). What is held is read
//! from the books (the more valuable sellable coin), so nothing is saved.
//! Evaluated once per close time when both candles are in, nothing is
//! working, and both histories line up bar for bar.
//!
//! Why it may work: two coins driven by the same market (BTC and ETH) move
//! together, and short-lived divergences in their ratio have tended to
//! close. How it fails: the ratio trends (ETH/BTC fell through most of
//! 2022–2025), and the strategy keeps switching into the coin that keeps
//! getting cheaper. Spot can't short, so this is not the classic hedged
//! pair trade: it is always fully exposed to the pair's common move, and
//! only the switch bets on the spread. Each switch pays two fees and two
//! slippages. Expected edge is unverified.
//!
//! The plain ratio, not its logarithm: `Decimal` has no exact log, and over
//! a short window the two differ little. Reads only closed candles through
//! `Context` (invariants 2, 5), in `Decimal` (invariant 1).

use super::portfolio::{self, Backoff, Target};
use super::rebalance::{completes, one_quote};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Notional, Side, Signal};
use serde::Deserialize;

const NAME: &str = "pair_reversion";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairReversionParams {
    /// The two coins, A then B (`["binance:BTCUSDT", "binance:ETHUSDT"]`),
    /// one quote asset. The ratio is A / B.
    coins: [InstrumentId; 2],
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
    /// Close times in the ratio's mean and spread, current included. At
    /// least 2; the engine's `history_len` must be at least this.
    period: usize,
    /// Distance from the mean, in standard deviations, that makes a coin
    /// cheap (`"2"`). Positive.
    #[serde(deserialize_with = "exact_decimal")]
    entry_z: Decimal,
    /// How far the held coin's weight may drift below all of capital
    /// before it is topped up (`"0.05"`). In (0, 1). A switch always
    /// trades.
    #[serde(deserialize_with = "exact_decimal")]
    band: Decimal,
    /// Fee per trade assumed (`"0.001"`): with slippage, discounts the
    /// sale's proceeds the switch's buy may spend.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`); match
    /// the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct PairReversion {
    p: PairReversionParams,
    stopped: bool,
    backoff: Backoff,
}

impl PairReversion {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: PairReversionParams = params.parse(NAME)?;
        let bad = |reason: String| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason,
            })
        };
        if let Err(e) = portfolio::check_coins(&p.coins, 2) {
            return bad(e);
        }
        if p.period < 2 {
            return bad("period must be >= 2".into());
        }
        if p.entry_z <= Decimal::ZERO {
            return bad("entry_z must be positive".into());
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

    /// The coin the ratio says is cheap: `Some(Some(i))` for coin i,
    /// `Some(None)` inside the band, `None` without `period` aligned bars.
    fn signal(&self, ctx: &Context) -> Option<Option<usize>> {
        let n = self.p.period;
        let [a, b] = &self.p.coins;
        let (ha, hb) = (ctx.history(a), ctx.history(b));
        if ha.len() < n || hb.len() < n {
            return None;
        }
        let mut ratios = Vec::with_capacity(n);
        for (x, y) in ha.rev().take(n).zip(hb.rev().take(n)) {
            if x.close_time != y.close_time {
                return None;
            }
            ratios.push(x.close.get() / y.close.get());
        }
        let count = Decimal::from(n);
        let mean = ratios.iter().sum::<Decimal>() / count;
        let var = ratios
            .iter()
            .map(|r| (*r - mean) * (*r - mean))
            .sum::<Decimal>()
            / count;
        // Newest first: ratios[0] is the current bar's.
        let d = ratios[0] - mean;
        if d.is_zero() || d * d <= self.p.entry_z * self.p.entry_z * var {
            return Some(None);
        }
        Some(Some(if d.is_sign_negative() { 0 } else { 1 }))
    }

    /// The coin held: the more valuable one whose lot-rounded holding a
    /// market sell could sell (dust doesn't count).
    fn held(&self, ctx: &Context) -> Option<usize> {
        let mut best: Option<(usize, Decimal)> = None;
        for (i, coin) in self.p.coins.iter().enumerate() {
            let (Some(inst), Some(last)) = (ctx.instrument(coin), ctx.last_candle(coin)) else {
                continue;
            };
            let units = inst.round_qty(ctx.position(coin).qty());
            if inst.check_market(units, last.close, Side::Sell).is_err() {
                continue;
            }
            let value = inst.notional(units, last.close).get();
            if best.is_none_or(|(_, v)| value > v) {
                best = Some((i, value));
            }
        }
        best.map(|(i, _)| i)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: PairReversion::build }
}

impl Strategy for PairReversion {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, &self.p.coins);
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: pair_reversion stopped"
            );
        }
    }

    /// What is held comes from the books, the ratio from history: both
    /// rebuilt on a restart, so only the format version is saved.
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
        let Some(signal) = self.signal(ctx) else {
            return Vec::new();
        };
        // Never switch into a coin scheduled for delisting; one held is
        // targeted at 0 by `portfolio::rebalance` and sold.
        let leaving = |i: &usize| ctx.delisting(&coins[*i]).is_some();
        let Some(target) = signal.filter(|i| !leaving(i)).or_else(|| self.held(ctx)) else {
            return Vec::new();
        };
        if self.backoff.hold() {
            return Vec::new();
        }
        let targets: Vec<Target> = coins
            .iter()
            .enumerate()
            .map(|(i, coin)| Target {
                coin: coin.clone(),
                weight: if i == target {
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                },
            })
            .collect();
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
    use bot_core::{OrderKind, Qty, Size};
    use rust_decimal_macros::dec;

    fn params_with(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               capital = "1000"
               period = 3
               entry_z = "1"
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), ethusdt()], params_with(""), 10)
    }

    /// Feeds bars (BTC, ETH) from `i`; returns the last bar's signals.
    fn bars(
        s: &mut dyn Strategy,
        ctx: &mut Context,
        from: i64,
        px: &[(&str, &str)],
    ) -> Vec<Signal> {
        let mut out = Vec::new();
        for (k, (b, e)) in px.iter().enumerate() {
            let i = from + k as i64;
            for c in [candle(i, b), eth_candle(i, e)] {
                driver::set_now(ctx, c.close_time);
                driver::push_candle(ctx, c.clone());
                out = s.on_candle(ctx, &c);
                out.extend(driver::take_signals(ctx));
            }
        }
        out
    }

    fn buy(id: &InstrumentId, budget: Decimal) -> Signal {
        Signal::Submit {
            instrument: id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(budget)),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params_with("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        let p = |coins: &str, z: &str, period: usize| {
            Params::from_toml(&format!(
                "coins = {coins}\ncapital = \"1000\"\nperiod = {period}\nentry_z = {z}\n\
                 band = \"0.05\"\nfee = \"0.001\"\nslippage_bps = \"10\""
            ))
            .unwrap()
        };
        let two = r#"["binance:BTCUSDT", "binance:ETHUSDT"]"#;
        assert!(build(NAME, &p(two, "\"1\"", 3)).is_ok());
        assert!(build(NAME, &p(r#"["binance:BTCUSDT"]"#, "\"1\"", 3)).is_err());
        assert!(
            build(
                NAME,
                &p(r#"["binance:BTCUSDT", "binance:BTCUSDT"]"#, "\"1\"", 3)
            )
            .is_err()
        );
        assert!(
            build(
                NAME,
                &p(
                    r#"["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]"#,
                    "\"1\"",
                    3
                )
            )
            .is_err(),
            "exactly two"
        );
        assert!(build(NAME, &p(two, "\"0\"", 3)).is_err());
        assert!(build(NAME, &p(two, "1.5", 3)).is_err(), "float");
        assert!(build(NAME, &p(two, "\"1\"", 1)).is_err());
    }

    /// Ratios (BTC/ETH) 10, 10, 10.4: mean 10.1333, deviations −0.1333 ×2,
    /// +0.2667 → var = (0.01778·2 + 0.07111)/3 = 0.035556, sd 0.18856.
    /// d = 0.2667 > 1·sd → B (ETH) is cheap. With entry_z 1.5: 0.2667 <
    /// 0.2828 → inside. 10, 10, 9.6 → A (BTC) cheap. 10, 10, 10 → inside.
    #[test]
    fn signal_picks_the_cheap_coin() {
        let check = |z: &str, last: &str| {
            let s = PairReversion {
                p: params_with("").parse(NAME).unwrap(),
                stopped: false,
                backoff: Backoff::default(),
            };
            let s = PairReversion {
                p: PairReversionParams {
                    entry_z: z.parse().unwrap(),
                    ..s.p
                },
                ..s
            };
            let mut ctx = ctx();
            for (i, b) in ["100", "100", last].into_iter().enumerate() {
                driver::push_candle(&mut ctx, candle(i as i64, b));
                driver::push_candle(&mut ctx, eth_candle(i as i64, "10"));
            }
            s.signal(&ctx)
        };
        assert_eq!(check("1", "104"), Some(Some(1)));
        assert_eq!(check("1.5", "104"), Some(None));
        assert_eq!(check("1", "96"), Some(Some(0)));
        assert_eq!(check("1", "100"), Some(None));
    }

    /// The ratio says ETH is cheap, but ETH is scheduled for delisting:
    /// nothing is bought (holding nothing, nothing to keep).
    #[test]
    fn never_switches_into_a_delisting_coin() {
        let mut s = build(NAME, &params_with("")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        driver::set_delisting(
            &mut ctx,
            ethusdt().id.clone(),
            Some(bot_core::Timestamp::from_millis(1 << 40)),
        );
        bars(
            s.as_mut(),
            &mut ctx,
            0,
            &[("100", "10"), ("100", "10"), ("100", "10")],
        );
        assert!(bars(s.as_mut(), &mut ctx, 3, &[("104", "10")]).is_empty());
    }

    /// Flat, no signal → nothing. Then BTC jumps: ETH cheap → buy ETH with
    /// all of capital. Holding ETH, BTC drops below the mean: switch, sell
    /// all ETH first, then buy BTC with the estimated proceeds.
    #[test]
    fn switches_into_the_cheap_coin() {
        let mut s = build(NAME, &params_with("")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        assert!(
            bars(
                s.as_mut(),
                &mut ctx,
                0,
                &[("100", "10"), ("100", "10"), ("100", "10")]
            )
            .is_empty()
        );
        assert_eq!(
            bars(s.as_mut(), &mut ctx, 3, &[("104", "10")]),
            vec![buy(&ethusdt().id, dec!(1000))]
        );
        driver::set_position(
            &mut ctx,
            position(&ethusdt().id, dec!(99.9), dec!(1000), dec!(0)),
        );
        // Ratios 10, 10.4, 10.4 → mean 10.2667, d = 0.1333 (inside at z 1,
        // sd 0.1886): keep ETH, nothing to trade.
        assert!(bars(s.as_mut(), &mut ctx, 4, &[("104", "10")]).is_empty());
        let got = bars(s.as_mut(), &mut ctx, 5, &[("96", "10")]);
        // ETH 99.9 at 10 = 999; cash 0; proceeds est. 99.9·(10·0.998 − 0.01)
        // (fee + slippage 0.002; this test ETH has BTC's 0.01 tick) = 996.003.
        assert_eq!(
            got,
            vec![
                Signal::Submit {
                    instrument: ethusdt().id.clone(),
                    side: Side::Sell,
                    size: Size::Base(Qty::new(dec!(99.9))),
                    kind: OrderKind::Market,
                },
                buy(&btcusdt().id, dec!(996.003)),
            ]
        );
    }

    /// Bars that don't line up (one coin's candle missing) → no decision.
    #[test]
    fn misaligned_histories_decide_nothing() {
        let s = PairReversion {
            p: params_with("").parse(NAME).unwrap(),
            stopped: false,
            backoff: Backoff::default(),
        };
        let mut ctx = ctx();
        for i in 0..3 {
            driver::push_candle(&mut ctx, candle(i, "100"));
        }
        for i in [0, 2, 3] {
            driver::push_candle(&mut ctx, eth_candle(i, "10"));
        }
        assert_eq!(s.signal(&ctx), None);
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with("")).unwrap();
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
