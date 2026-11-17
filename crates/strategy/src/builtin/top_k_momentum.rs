//! `top_k_momentum`: hold the `k` coins with the best recent return, each
//! at 1/k of capital.
//!
//! Each close time, once every coin's candle is in and nothing is working:
//! every coin's momentum ranks the coins (ties: the one listed first). The
//! best `k` whose momentum beats `min_momentum` (and pass the trend filter,
//! if set) get their slots; the rest 0. A slot no coin qualifies for stays
//! cash. Holdings are traded to those weights of `capital` with market
//! orders once any is more than `band` off, through the shared `portfolio`
//! code (own cash from the books, sells first, their proceeds fund the
//! buys). So a coin leaving the top k is sold and the one entering bought
//! in the same evaluation.
//!
//! Optional overlays, each off by default:
//!
//! - **Absolute momentum** (`trend_period`): a coin is held only while its
//!   close is above its `trend_period`-bar SMA, so in a broad fall the
//!   slots go to cash instead of to the least-bad loser. `min_momentum`
//!   is the other absolute test: set it to a T-bill's return over the
//!   lookback to demand more than cash pays. Cash is the only refuge; a
//!   bond ETF can be listed as one of the coins.
//! - **Several lookbacks** (`lookbacks = [21, 63, 126]` instead of
//!   `lookback`): momentum is the mean of the returns over each, so a
//!   short window can demote a breaking leader before a long one would.
//! - **Inverse-volatility weights** (`weighting = "inverse_vol"`,
//!   `vol_period`): the chosen coins share their slots in proportion to
//!   1 / sd of their last `vol_period` returns, so the most erratic holds
//!   least. Relative only: when every coin gets wilder the total held is
//!   unchanged (`vol_target` / `tsmom` cut total exposure).
//! - **Scheduled rebalancing with a trailing stop** (`rebalance_every`,
//!   `trail_pct`): ranking and rebalancing happen only at the first
//!   evaluation of each UTC hour / day / week / month. Between them, a held
//!   coin whose close falls `trail_pct` below its highest close since the
//!   last rebalance is sold whole, and its slot stays cash until the next
//!   one. The peak is taken from history, so `history_len` must cover a
//!   rebalance period's bars.
//!
//! `momentum_rotation` with k slots instead of one: less riding on a single
//! coin's reversal, at the cost of more trades and a diluted best pick.
//! Bets recent winners keep winning (cross-sectional momentum); fails on
//! sharp reversals and when leadership churns (each change of the top k is
//! a round trip of fees and slippage). The overlays trade some return in
//! rising markets for less in falling ones; a stop sells into the dip it
//! measures and the next rebalance may buy back higher. Coins chosen with
//! hindsight inflate any backtest of it. Expected edge is unverified.
//!
//! State: the time of the last rebalance (with `rebalance_every`), so a
//! restart neither rebalances twice in a period nor loses the stop's
//! window; everything else is rebuilt from positions and history. Reads
//! only closed candles through `Context` (invariants 2, 5), in `Decimal`
//! (invariant 1). All coins must share one quote asset, else it never
//! trades (logged).

use super::calendar::{Period, period_start};
use super::long_flat::long_units;
use super::portfolio::{self, Backoff, Target};
use super::rebalance::{completes, one_quote};
use crate::{
    Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal, indicator,
};
use bot_core::{
    Candle, Decimal, Fill, InstrumentId, Notional, OrderKind, Side, Signal, Size, Timestamp,
};
use serde::Deserialize;

const NAME: &str = "top_k_momentum";

/// How the chosen coins share their slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Weighting {
    /// 1/k each.
    #[default]
    Equal,
    /// In proportion to 1 / volatility, over the chosen coins' slots.
    InverseVol,
}

/// An optional exact decimal (`trail_pct = "0.1"`); absent = `None`.
fn opt_decimal<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
    exact_decimal(d).map(Some)
}

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
    /// least that. Exactly one of `lookback` and `lookbacks`.
    #[serde(default)]
    lookback: Option<usize>,
    /// Several windows (`[21, 63, 126]`): momentum = the mean of their
    /// returns. Each at least 1.
    #[serde(default)]
    lookbacks: Option<Vec<usize>>,
    /// A coin's momentum must be above this to be held (`"0.02"` = 2%).
    /// Default `"0"`: only rising coins. Above −1.
    #[serde(deserialize_with = "exact_decimal", default)]
    min_momentum: Decimal,
    /// If set, a coin is held only while its close is above the simple
    /// mean of its last `trend_period` closes (current included). At
    /// least 2.
    #[serde(default)]
    trend_period: Option<usize>,
    /// `"equal"` (default) or `"inverse_vol"` (needs `vol_period`).
    #[serde(default)]
    weighting: Weighting,
    /// Returns the volatility is measured over (`inverse_vol`); at least 2.
    #[serde(default)]
    vol_period: Option<usize>,
    /// Rank and rebalance only at the first evaluation of each `"hour"`,
    /// `"day"`, `"week"` (from Monday) or `"month"` (UTC). Default: every
    /// evaluation.
    #[serde(default)]
    rebalance_every: Option<Period>,
    /// Trailing stop between rebalances (`"0.1"` = 10% below the highest
    /// close since the last one); needs `rebalance_every`. In (0, 1).
    #[serde(deserialize_with = "opt_decimal", default)]
    trail_pct: Option<Decimal>,
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
    /// The return windows, from `lookback` or `lookbacks`.
    windows: Vec<usize>,
    stopped: bool,
    backoff: Backoff,
    /// Close time of the evaluation that last ranked and rebalanced.
    rebalanced_at: Option<Timestamp>,
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
        let windows = match (p.lookback, &p.lookbacks) {
            (Some(n), None) => vec![n],
            (None, Some(v)) if !v.is_empty() => v.clone(),
            _ => return bad("set one of lookback and lookbacks (non-empty)".into()),
        };
        if windows.contains(&0) {
            return bad("lookback must be >= 1".into());
        }
        if p.min_momentum <= -Decimal::ONE {
            return bad("min_momentum must be above -1".into());
        }
        if p.trend_period.is_some_and(|n| n < 2) {
            return bad("trend_period must be >= 2".into());
        }
        match (p.weighting, p.vol_period) {
            (Weighting::InverseVol, Some(n)) if n >= 2 => {}
            (Weighting::InverseVol, _) => {
                return bad("weighting = \"inverse_vol\" needs vol_period >= 2".into());
            }
            (Weighting::Equal, Some(_)) => {
                return bad("vol_period is used only with weighting = \"inverse_vol\"".into());
            }
            (Weighting::Equal, None) => {}
        }
        if let Some(t) = p.trail_pct {
            if t <= Decimal::ZERO || t >= Decimal::ONE {
                return bad("trail_pct must be in (0, 1)".into());
            }
            if p.rebalance_every.is_none() {
                return bad("trail_pct needs rebalance_every (the stop's window)".into());
            }
        }
        if let Err(e) = portfolio::check_common(p.capital, p.band, p.fee, p.slippage_bps) {
            return bad(e.into());
        }
        Ok(Box::new(Self {
            p,
            windows,
            stopped: false,
            backoff: Backoff::default(),
            rebalanced_at: None,
        }))
    }

    /// Closes every coin needs before anything is decided.
    fn bars_needed(&self) -> usize {
        let longest = self.windows.iter().max().copied().unwrap_or(1) + 1;
        let vol = match self.p.weighting {
            Weighting::InverseVol => self.p.vol_period.unwrap_or(2) + 1,
            Weighting::Equal => 0,
        };
        longest.max(self.p.trend_period.unwrap_or(0)).max(vol)
    }

    /// The slots: each of the best `k` coins whose momentum beats
    /// `min_momentum` and whose close is above its trend average (if set)
    /// gets 1/k, or with `inverse_vol` the chosen coins share their
    /// slots by 1 / volatility; the rest 0. `None` until every coin has
    /// enough closes, or while a chosen coin's volatility is 0 (no
    /// meaningful inverse). A coin scheduled for delisting, or halted,
    /// isn't ranked: its slot goes to the next.
    fn targets(&self, ctx: &Context) -> Option<Vec<Target>> {
        let need = self.bars_needed();
        let mut ranked: Vec<(usize, Decimal, bool)> = self
            .p
            .coins
            .iter()
            .enumerate()
            .filter(|(_, coin)| ctx.delisting(coin).is_none() && !ctx.halted(coin))
            .map(|(i, coin)| {
                let hist = ctx.history(coin);
                if hist.len() < need {
                    return None;
                }
                // Newest first.
                let closes: Vec<Decimal> = hist.rev().take(need).map(|c| c.close.get()).collect();
                let sum: Decimal = self
                    .windows
                    .iter()
                    .map(|&w| closes[0] / closes[w] - Decimal::ONE)
                    .sum();
                let momentum = sum / Decimal::from(self.windows.len());
                let trending = self.p.trend_period.is_none_or(|n| {
                    let mean = closes[..n].iter().sum::<Decimal>() / Decimal::from(n);
                    closes[0] > mean
                });
                Some((i, momentum, trending))
            })
            .collect::<Option<_>>()?;
        // Best first; a stable sort keeps list order among equals.
        ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
        let chosen: Vec<usize> = ranked
            .iter()
            .take(self.p.k)
            .filter(|(_, m, trending)| *m > self.p.min_momentum && *trending)
            .map(|(i, _, _)| *i)
            .collect();
        let slot = Decimal::ONE / Decimal::from(self.p.k);
        let mut weights = vec![Decimal::ZERO; self.p.coins.len()];
        match self.p.weighting {
            Weighting::Equal => chosen.iter().for_each(|&i| weights[i] = slot),
            Weighting::InverseVol => {
                let period = self.p.vol_period?;
                let inverse: Vec<(usize, Decimal)> = chosen
                    .iter()
                    .map(|&i| {
                        let sd = indicator::return_sd(ctx.history(&self.p.coins[i]), period)?;
                        (!sd.is_zero()).then(|| (i, Decimal::ONE / sd))
                    })
                    .collect::<Option<_>>()?;
                let total: Decimal = inverse.iter().map(|(_, v)| *v).sum();
                let held = slot * Decimal::from(chosen.len());
                for (i, v) in inverse {
                    weights[i] = held * v / total;
                }
            }
        }
        Some(
            self.p
                .coins
                .iter()
                .zip(weights)
                .map(|(coin, weight)| Target {
                    coin: coin.clone(),
                    weight,
                })
                .collect(),
        )
    }

    /// Whether this evaluation ranks and rebalances: always without
    /// `rebalance_every`, else the first one in a new period.
    fn due(&self, now: Timestamp) -> bool {
        match (self.p.rebalance_every, self.rebalanced_at) {
            (Some(every), Some(at)) => period_start(every, at) != period_start(every, now),
            _ => true,
        }
    }

    /// Between rebalances: a market sell of every held coin whose close
    /// is `trail_pct` or more below its highest close since the last
    /// rebalance (that evaluation's bar included).
    fn stops(&self, ctx: &Context) -> Vec<Signal> {
        let (Some(trail), Some(since)) = (self.p.trail_pct, self.rebalanced_at) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for coin in self.p.coins.iter().filter(|c| ctx.tradable(c)) {
            let Some(last) = ctx.last_candle(coin) else {
                continue;
            };
            let Some(units) = long_units(ctx, coin, last) else {
                continue;
            };
            let peak = ctx
                .history(coin)
                .filter(|c| c.close_time >= since)
                .map(|c| c.close.get())
                .max()
                .unwrap_or(last.close.get());
            if last.close.get() <= peak * (Decimal::ONE - trail) {
                out.push(Signal::Submit {
                    instrument: coin.clone(),
                    side: Side::Sell,
                    size: Size::Base(units),
                    kind: OrderKind::Market,
                });
            }
        }
        out
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

    /// The last rebalance's time (`null` until one): the schedule and the
    /// stop's window. Positions and history are rebuilt on a restart.
    fn save_state(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "v": super::STATE_VERSION,
            "rebalanced_at": self.rebalanced_at.map(Timestamp::as_millis),
        }))
    }

    /// Takes the saved time as is; state from before it was saved (no
    /// `rebalanced_at`) means "never": the next evaluation rebalances.
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        self.rebalanced_at = match &state["rebalanced_at"] {
            serde_json::Value::Null => None,
            v => {
                Some(Timestamp::from_millis(v.as_i64().ok_or_else(|| {
                    super::state_error(NAME, "bad rebalanced_at")
                })?))
            }
        };
        Ok(())
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
        if !self.due(c.close_time) {
            if self.backoff.hold() {
                return Vec::new();
            }
            return self.stops(ctx);
        }
        let Some(targets) = self.targets(ctx) else {
            return Vec::new();
        };
        if self.backoff.hold() {
            return Vec::new();
        }
        let cost = portfolio::trade_cost(self.p.fee, self.p.slippage_bps);
        match portfolio::rebalance(ctx, &targets, self.p.capital, self.p.band, cost) {
            Some(out) => {
                self.rebalanced_at = Some(c.close_time);
                out
            }
            None => Vec::new(),
        }
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
        assert_eq!(state, serde_json::json!({ "v": 1, "rebalanced_at": null }));
        let ctx = ctx();
        assert!(s.restore_state(state, &ctx).is_ok());
        assert!(
            s.restore_state(serde_json::json!({ "v": 2 }), &ctx)
                .is_err()
        );
    }

    /// Params with every key given in `body` (no default lookback).
    fn raw(k: usize, body: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
               k = {k}
               capital = "3000"
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {body}"#
        ))
        .unwrap()
    }

    fn sell(id: &InstrumentId, q: Decimal) -> Signal {
        Signal::Submit {
            instrument: id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn rejects_bad_overlay_params() {
        for bad in [
            "",                                 // no lookback at all
            "lookback = 1\nlookbacks = [1, 2]", // both
            "lookbacks = []",
            "lookbacks = [1, 0]",
            "lookback = 1\ntrend_period = 1",
            "lookback = 1\nweighting = \"inverse_vol\"", // no vol_period
            "lookback = 1\nweighting = \"inverse_vol\"\nvol_period = 1",
            "lookback = 1\nvol_period = 5",      // unused
            "lookback = 1\ntrail_pct = \"0.1\"", // no schedule
            "lookback = 1\nrebalance_every = \"day\"\ntrail_pct = \"1\"",
            "lookback = 1\nrebalance_every = \"year\"",
        ] {
            assert!(build(NAME, &raw(1, bad)).is_err(), "{bad:?} accepted");
        }
        let all = "lookbacks = [1, 2]\ntrend_period = 3\nweighting = \"inverse_vol\"
                   vol_period = 2\nrebalance_every = \"month\"\ntrail_pct = \"0.1\"";
        assert!(build(NAME, &raw(1, all)).is_ok());
    }

    /// BTC 100, 100, 110; ETH 10, 13, 12; SOL 50, 40, 46.
    /// 1-bar returns: BTC 0.1, ETH 12/13 − 1 = −0.077, SOL 0.15 → SOL.
    /// 2-bar returns: BTC 0.1, ETH 0.2, SOL −0.08.
    /// Mean of both: BTC 0.1, ETH 0.0615, SOL 0.035 → BTC, unlike either.
    #[test]
    fn several_lookbacks_rank_by_their_mean_return() {
        let px = [
            ("100", "10", "50"),
            ("100", "13", "40"),
            ("110", "12", "46"),
        ];
        for (body, winner) in [
            ("lookback = 1", solusdt().id.clone()),
            ("lookback = 2", ethusdt().id.clone()),
            ("lookbacks = [1, 2]", btcusdt().id.clone()),
        ] {
            let mut s = build(NAME, &raw(1, body)).unwrap();
            let mut ctx = ctx();
            s.on_start(&mut ctx);
            let got = bars(s.as_mut(), &mut ctx, 0, &px);
            assert_eq!(got, vec![buy(&winner, dec!(3000))], "{body}");
        }
    }

    /// Bars: BTC 100, 100, 105; ETH 20, 10, 11; SOL 50, 51, 52. 1-bar
    /// returns ETH 0.1, BTC 0.05, SOL 0.0196 → top 2 = ETH, BTC. ETH's
    /// close 11 is under its 3-bar mean 41/3 = 13.67: with the trend
    /// filter its slot is cash (not handed to SOL); BTC (105 > 101.67)
    /// keeps its 1500.
    #[test]
    fn the_trend_filter_turns_a_falling_leader_into_cash() {
        let px = [
            ("100", "20", "50"),
            ("100", "10", "51"),
            ("105", "11", "52"),
        ];
        let run = |body: &str| {
            let mut s = build(NAME, &raw(2, body)).unwrap();
            let mut ctx = ctx();
            s.on_start(&mut ctx);
            bars(s.as_mut(), &mut ctx, 0, &px)
        };
        assert_eq!(
            run("lookback = 1"),
            vec![
                buy(&btcusdt().id, dec!(1500)),
                buy(&ethusdt().id, dec!(1500))
            ]
        );
        assert_eq!(
            run("lookback = 1\ntrend_period = 3"),
            vec![buy(&btcusdt().id, dec!(1500))]
        );
    }

    /// BTC 100, 110, 99 (returns 0.1, −0.1); ETH 100, 105, 99.75 (0.05,
    /// −0.05); SOL 100, 100, 80. Last return: ETH −0.05, BTC −0.1, SOL
    /// −0.2 → ETH, BTC (min_momentum −0.5). Sample sd over 2 returns =
    /// |r₁ − r₂| / √2: BTC 0.2/√2, ETH 0.1/√2, so by 1/sd ETH gets 2/3 of
    /// the two slots, BTC 1/3: 2000 and 1000 of 3000. The square root is
    /// approximate: equal to 1e-6.
    #[test]
    fn inverse_vol_gives_the_calmer_coin_more() {
        let body =
            "lookback = 1\nmin_momentum = \"-0.5\"\nweighting = \"inverse_vol\"\nvol_period = 2";
        let mut s = build(NAME, &raw(2, body)).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        let got = bars(
            s.as_mut(),
            &mut ctx,
            0,
            &[
                ("100", "100", "100"),
                ("110", "105", "100"),
                ("99", "99.75", "80"),
            ],
        );
        let budgets: Vec<(InstrumentId, Decimal)> = got
            .iter()
            .map(|g| match g {
                Signal::Submit {
                    instrument,
                    side: Side::Buy,
                    size: Size::Quote(n),
                    ..
                } => (instrument.clone(), n.get()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(budgets.len(), 2);
        assert_eq!(
            (&budgets[0].0, &budgets[1].0),
            (&btcusdt().id, &ethusdt().id)
        );
        assert!(
            (budgets[0].1 - dec!(1000)).abs() < dec!(0.000001),
            "{budgets:?}"
        );
        assert!(
            (budgets[1].1 - dec!(2000)).abs() < dec!(0.000001),
            "{budgets:?}"
        );
    }

    /// Daily schedule on hourly bars, 10% trailing stop, k = 1:
    /// - bar 1 (BTC +10%) ranks and buys BTC with 3000;
    /// - held 27 BTC (cost 2970); bar 2: ETH doubles but it's the same
    ///   day: no re-rank; BTC 120 is its peak: no stop;
    /// - bar 3: BTC 108 = 120 · 0.9 → sell all 27;
    /// - flat (realized 27 · 108 − 2970 = −54); bar 4: BTC 130, same day:
    ///   the slot stays cash;
    /// - bar 24 (next day): BTC 131 vs 130 is the best 1-bar return (ETH,
    ///   SOL flat) → buy BTC with own cash 3000 − (0 − (−54)) = 2946.
    #[test]
    fn scheduled_rebalances_with_a_trailing_stop_between_them() {
        let body = "lookback = 1\nrebalance_every = \"day\"\ntrail_pct = \"0.1\"";
        let mut s = build(NAME, &raw(1, body)).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        let got = bars(s.as_mut(), &mut ctx, 1, &[("110", "10", "50")]);
        assert_eq!(got, vec![buy(&btcusdt().id, dec!(3000))]);
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(27), dec!(2970), dec!(0)),
        );
        assert!(bars(s.as_mut(), &mut ctx, 2, &[("120", "20", "50")]).is_empty());
        let got = bars(s.as_mut(), &mut ctx, 3, &[("108", "20", "50")]);
        assert_eq!(got, vec![sell(&btcusdt().id, dec!(27))]);
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(0), dec!(0), dec!(-54)),
        );
        assert!(bars(s.as_mut(), &mut ctx, 4, &[("130", "20", "50")]).is_empty());
        let got = bars(s.as_mut(), &mut ctx, 24, &[("131", "20", "50")]);
        assert_eq!(got, vec![buy(&btcusdt().id, dec!(2946))]);
    }

    /// The rebalance time survives a restart: restored mid-day, the
    /// schedule waits for the next day and the stop keeps its window.
    #[test]
    fn restores_the_last_rebalance() {
        let body = "lookback = 1\nrebalance_every = \"day\"\ntrail_pct = \"0.1\"";
        let mut s = build(NAME, &raw(1, body)).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        bars(s.as_mut(), &mut ctx, 0, &[("100", "10", "50")]);
        bars(s.as_mut(), &mut ctx, 1, &[("110", "10", "50")]);
        let state = s.save_state().unwrap();
        assert_eq!(state["rebalanced_at"], 2 * 3_600_000 - 1);

        let mut fresh = build(NAME, &raw(1, body)).unwrap();
        fresh.restore_state(state, &ctx).unwrap();
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(27), dec!(2970), dec!(0)),
        );
        // ETH +100% the same day: no re-rank; BTC 99 ≤ 110 · 0.9: stop.
        let got = bars(fresh.as_mut(), &mut ctx, 2, &[("99", "20", "50")]);
        assert_eq!(got, vec![sell(&btcusdt().id, dec!(27))]);
        assert!(
            fresh
                .restore_state(serde_json::json!({ "v": 1, "rebalanced_at": "x" }), &ctx)
                .is_err()
        );
    }
}
