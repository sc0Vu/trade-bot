//! `top_k_momentum`: hold the `k` coins with the best recent return, each
//! at 1/k of capital.
//!
//! Each close time, once every coin's candle is in and nothing is working:
//! every coin's return over `lookback` bars, `close / close[lookback bars
//! ago] − 1`, ranks the coins (ties: the one listed first). The best `k`
//! whose return beats `min_momentum` get weight 1/k each; the rest 0. A
//! slot no coin qualifies for stays cash. Holdings are traded to those
//! weights of `capital` with market orders once any is more than `band`
//! off, through the shared `portfolio` code (own cash from the books,
//! sells first, their proceeds fund the buys). So a coin leaving the top k
//! is sold and the one entering bought in the same evaluation.
//!
//! `momentum_rotation` with k slots instead of one: less riding on a single
//! coin's reversal, at the cost of more trades and a diluted best pick.
//! Bets recent winners keep winning (cross-sectional momentum); fails on
//! sharp reversals and when leadership churns (each change of the top k is
//! a round trip of fees and slippage). Coins chosen with hindsight inflate
//! any backtest of it. Expected edge is unverified.
//!
//! Stateless: positions and history are all a decision needs, both rebuilt
//! on a restart. Reads only closed candles through `Context` (invariants
//! 2, 5), in `Decimal` (invariant 1). All coins must share one quote
//! asset, else it never trades (logged).

use super::portfolio::{self, Backoff, Target};
use super::rebalance::{completes, one_quote};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "top_k_momentum";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TopKMomentumParams {
    /// Coins ranked (`["binance:BTCUSDT", "binance:ETHUSDT",
    /// "binance:SOLUSDT"]`), one quote asset; more than `k`.
    coins: Vec<InstrumentId>,
    /// How many coins are held at most; each gets 1/k of `capital`. At
    /// least 1, fewer than the coins.
    k: usize,
    /// Bars the return is measured over; at least 1. Each coin needs
    /// `lookback + 1` closes, so the engine's `history_len` must be at
    /// least that.
    lookback: usize,
    /// A coin must return more than this over `lookback` bars to be held
    /// (`"0.02"` = 2%). Default `"0"`: only rising coins. Above −1.
    #[serde(deserialize_with = "exact_decimal", default)]
    min_momentum: Decimal,
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
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
pub struct TopKMomentum {
    p: TopKMomentumParams,
    stopped: bool,
    backoff: Backoff,
}

impl TopKMomentum {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: TopKMomentumParams = params.parse(NAME)?;
        let bad = |reason: String| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason,
            })
        };
        if let Err(e) = portfolio::check_coins(&p.coins, 2) {
            return bad(e);
        }
        if p.k == 0 || p.k >= p.coins.len() {
            return bad("k must be at least 1 and fewer than the coins".into());
        }
        if p.lookback == 0 {
            return bad("lookback must be >= 1".into());
        }
        if p.min_momentum <= -Decimal::ONE {
            return bad("min_momentum must be above -1".into());
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

    /// 1/k for each of the best `k` coins beating `min_momentum`, 0 for the
    /// rest; `None` until every coin has `lookback + 1` closes. A coin
    /// scheduled for delisting, or halted, isn't ranked: its slot goes to
    /// the next.
    fn targets(&self, ctx: &Context) -> Option<Vec<Target>> {
        let mut ranked: Vec<(usize, Decimal)> = self
            .p
            .coins
            .iter()
            .enumerate()
            .filter(|(_, coin)| ctx.delisting(coin).is_none() && !ctx.halted(coin))
            .map(|(i, coin)| {
                let hist = ctx.history(coin);
                if hist.len() < self.p.lookback + 1 {
                    return None;
                }
                let closes: Vec<Decimal> = hist
                    .rev()
                    .take(self.p.lookback + 1)
                    .map(|c| c.close.get())
                    .collect();
                Some((i, closes[0] / closes[self.p.lookback] - Decimal::ONE))
            })
            .collect::<Option<_>>()?;
        // Best first; a stable sort keeps list order among equals.
        ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
        let chosen: Vec<usize> = ranked
            .iter()
            .take(self.p.k)
            .filter(|(_, r)| *r > self.p.min_momentum)
            .map(|(i, _)| *i)
            .collect();
        let share = Decimal::ONE / Decimal::from(self.p.k);
        Some(
            self.p
                .coins
                .iter()
                .enumerate()
                .map(|(i, coin)| Target {
                    coin: coin.clone(),
                    weight: if chosen.contains(&i) {
                        share
                    } else {
                        Decimal::ZERO
                    },
                })
                .collect(),
        )
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: TopKMomentum::build }
}

impl Strategy for TopKMomentum {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, &self.p.coins);
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: top_k_momentum stopped"
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
        let Some(targets) = self.targets(ctx) else {
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
    use bot_core::{Asset, Instrument, InstrumentSpec, OrderKind, Qty, Side, Size};
    use rust_decimal_macros::dec;

    fn solusdt() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:SOLUSDT".parse().unwrap(),
            base: Asset::new("SOL").unwrap(),
            ..btcusdt().into()
        })
        .unwrap()
    }

    fn sol_candle(i: i64, price: &str) -> Candle {
        Candle::new(bot_core::CandleSpec {
            instrument: solusdt().id.clone(),
            ..candle(i, price).into()
        })
        .unwrap()
    }

    fn params_with(k: usize, extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
               k = {k}
               lookback = 1
               capital = "3000"
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), ethusdt(), solusdt()], Params::default(), 10)
    }

    /// Feeds bars (BTC, ETH, SOL) from `from`; returns the last bar's
    /// signals.
    fn bars(
        s: &mut dyn Strategy,
        ctx: &mut Context,
        from: i64,
        px: &[(&str, &str, &str)],
    ) -> Vec<Signal> {
        let mut out = Vec::new();
        for (n, (b, e, so)) in px.iter().enumerate() {
            let i = from + n as i64;
            for c in [candle(i, b), eth_candle(i, e), sol_candle(i, so)] {
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
        assert_eq!(build(NAME, &params_with(2, "")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(build(NAME, &params_with(0, "")).is_err());
        assert!(build(NAME, &params_with(3, "")).is_err(), "k = coins");
        assert!(build(NAME, &params_with(2, r#"min_momentum = "-1""#)).is_err());
        assert!(
            build(NAME, &params_with(2, "min_momentum = 0.1")).is_err(),
            "float"
        );
        assert!(build(NAME, &params_with(2, r#"min_momentum = "-0.5""#)).is_ok());
    }

    /// Returns over one bar: BTC +10%, ETH +20%, SOL +5% → top 2 = ETH,
    /// BTC, each 1/2 of 3000. With min_momentum 0.15 only ETH qualifies:
    /// one slot stays cash. Ties (all +10%) go to the coins listed first.
    #[test]
    fn holds_the_best_k_that_beat_the_floor() {
        let mut s = build(NAME, &params_with(2, "")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        assert!(bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]).is_empty());
        let got = bars(s.as_mut(), &mut ctx, 1, &[("110", "12", "52.5")]);
        assert_eq!(
            got,
            vec![
                buy(&btcusdt().id, dec!(1500)),
                buy(&ethusdt().id, dec!(1500))
            ]
        );

        let mut s = build(NAME, &params_with(2, r#"min_momentum = "0.15""#)).unwrap();
        let mut ctx = self::ctx();
        s.on_start(&mut ctx);
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        let got = bars(s.as_mut(), &mut ctx, 1, &[("110", "12", "52.5")]);
        assert_eq!(got, vec![buy(&ethusdt().id, dec!(1500))]);

        let mut s = build(NAME, &params_with(2, "")).unwrap();
        let mut ctx = self::ctx();
        s.on_start(&mut ctx);
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        let got = bars(s.as_mut(), &mut ctx, 1, &[("110", "11", "55")]);
        assert_eq!(
            got,
            vec![
                buy(&btcusdt().id, dec!(1500)),
                buy(&ethusdt().id, dec!(1500))
            ]
        );

        // ETH (+20%, the best) scheduled for delisting: not ranked, so the
        // two slots go to BTC (+10%) and SOL (+5%).
        let mut s = build(NAME, &params_with(2, "")).unwrap();
        let mut ctx = self::ctx();
        s.on_start(&mut ctx);
        driver::set_delisting(
            &mut ctx,
            ethusdt().id.clone(),
            Some(bot_core::Timestamp::from_millis(1 << 40)),
        );
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        let got = bars(s.as_mut(), &mut ctx, 1, &[("110", "12", "52.5")]);
        assert_eq!(
            got,
            vec![
                buy(&btcusdt().id, dec!(1500)),
                buy(&solusdt().id, dec!(1500))
            ]
        );
    }

    /// Holding BTC and ETH at half each; SOL overtakes BTC: sell BTC, buy
    /// SOL with the sales' estimated proceeds. Once any coin leaves the
    /// band every coin goes back to target, so ETH (a bit over) is trimmed.
    #[test]
    fn rotates_the_coin_that_drops_out() {
        let mut s = build(NAME, &params_with(2, "")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(15), dec!(1500), dec!(0)),
        );
        driver::set_position(
            &mut ctx,
            position(&ethusdt().id, dec!(150), dec!(1500), dec!(0)),
        );
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        // BTC flat, ETH +1%, SOL +2%: top 2 = SOL, ETH. Value: BTC 1500,
        // ETH 1515, cash 0 → total 3015, targets 1507.5 each.
        let got = bars(s.as_mut(), &mut ctx, 1, &[("100", "10.1", "51")]);
        // Sell all 15 BTC; ETH 7.5 over → 7.5 / 10.1 → 0.74257 (lot).
        // Est. proceeds 15·(100·0.998 − 0.01) + 0.74257·(10.1·0.998 − 0.01)
        // = 1504.32753138 (8 dp, Python decimal) < SOL's 1507.5 → scaled.
        let sell = |id: &InstrumentId, q| Signal::Submit {
            instrument: id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        };
        assert_eq!(
            got,
            vec![
                sell(&btcusdt().id, dec!(15)),
                sell(&ethusdt().id, dec!(0.74257)),
                buy(&solusdt().id, dec!(1504.32753138)),
            ]
        );
    }

    #[test]
    fn saves_only_its_version() {
        let mut s = build(NAME, &params_with(2, "")).unwrap();
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
