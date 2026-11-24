//! `long_short_momentum`: cross-sectional momentum, market-neutral, for
//! instruments that can be shorted (USDⓈ-M perpetuals).
//!
//! Once per close time, with every coin's candle in and nothing of ours
//! working: rank the coins by their return over `lookback` bars, go long
//! the best `k` and short the worst `k`, each at `capital` / (2k) of
//! notional at the deciding close (lot-rounded); every other coin flat.
//! Gross exposure is `capital`, net about zero, so the market's direction
//! largely cancels and what is left is the spread between winners and
//! losers. Ties rank in listed order (a tie at the bottom shorts the later
//! one). Coins delisting or halted are not ranked; a delisting one is
//! closed (reduce-only), a halted one waits. Fewer than 2k rankable coins
//! → as many pairs as fit, same size each. On spot the short side is flat
//! (a long-only top k with half the capital: not neutral).
//!
//! With `rebalance_bars` = n it ranks only on bars closing at a multiple
//! of n bars since the epoch. Turnover is what decides this strategy:
//! BTC and 9 other perps, 4h, 2022–2026, k 2, lookback 84, deciding every
//! bar, made +157% before fees and slippage and −5% after (8,961 fills).
//! Walk-forward (`sweep`, 365-day train / 90-day test, 2023–2026, 2x
//! cross): with `rebalance_bars` swept in {6, 42}, picks chained +88% out
//! of sample against +26% for the median combination (beat it in 11 of 15
//! windows); deciding every bar (2x isolated), picks −57%. The 10 coins are today's
//! survivors (chosen in 2026): that flatters a winners-minus-losers bet.
//!
//! Orders: `builtin::signed` (shared with `tsmom_long_short`): a flip
//! closes reduce-only first and opens the other side at the next decision,
//! exits and cuts are reduce-only, a same-side resize only beyond `band`.
//! Reductions are emitted before openings, so freed margin comes first.
//! Sizes come from `capital`, not the wallet (invariant 5): keep it under
//! what the wallet can margin after losses. Stateless.
//!
//! Why it might hold: relative momentum across assets is one of the more
//! replicated anomalies in equities. Why it might not, here: crypto coins
//! move together (the spread is small next to each coin's own trend),
//! funding is paid on both legs (a short receives positive rates, a long
//! pays, so with funding mostly positive the short leg earns some back),
//! and every rotation is four taker fills with slippage. Edge unverified:
//! judge it with `trade-bot sweep`.

use super::portfolio;
use super::rebalance::{completes, one_quote};
use super::signed;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "long_short_momentum";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongShortMomentumParams {
    /// The universe ranked, one quote asset
    /// (`["binance-usdm:BTCUSDT", "binance-usdm:ETHUSDT", ...]`).
    coins: Vec<InstrumentId>,
    /// Coins per side; at least 1, and 2k ≤ coins.
    k: usize,
    /// Bars the ranking return is measured over (`42` = 7 days of 4h); at
    /// least 1, below `history_len`.
    lookback: usize,
    /// Rank and trade only on bars whose close is a multiple of this many
    /// bars since the epoch (`6` on 4h = once a day, at 00:00 UTC);
    /// default 1, every bar. Fewer decisions, fewer rank-edge swaps: the
    /// cost that decides this strategy. Counted from the epoch, not from
    /// the start, so it needs no state and a restart keeps the schedule.
    #[serde(default = "every_bar")]
    rebalance_bars: i64,
    /// Gross notional across both sides (`"2000"`): each position gets
    /// capital / (2k). Notional, not margin.
    capital: Notional,
    /// Resize a held position only beyond this fraction of its target
    /// (`"0.2"`). In [0, 1).
    #[serde(deserialize_with = "exact_decimal")]
    band: Decimal,
    /// Fee per trade assumed; required, range-checked, unused in decisions.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed (bps); required, unused.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

fn every_bar() -> i64 {
    1
}

/// See the module docs.
pub struct LongShortMomentum {
    p: LongShortMomentumParams,
    /// Set at `on_start` when the coins don't share one quote asset.
    stopped: bool,
}

impl LongShortMomentum {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: LongShortMomentumParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if let Err(e) = portfolio::check_coins(&p.coins, 2) {
            return bad(&e);
        }
        if p.k == 0 || 2 * p.k > p.coins.len() {
            return bad("k must be >= 1 with 2k <= coins");
        }
        if p.lookback == 0 {
            return bad("lookback must be >= 1");
        }
        if p.rebalance_bars < 1 {
            return bad("rebalance_bars must be >= 1");
        }
        if !p.capital.is_positive() {
            return bad("capital must be positive");
        }
        if p.band < Decimal::ZERO || p.band >= Decimal::ONE {
            return bad("band must be in [0, 1)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self { p, stopped: false }))
    }

    /// Return over `lookback` bars at `id`'s newest close; `None` without
    /// `lookback + 1` closes.
    fn momentum(&self, ctx: &Context, id: &InstrumentId) -> Option<Decimal> {
        let hist = ctx.history(id);
        if hist.len() < self.p.lookback + 1 {
            return None;
        }
        let mut rev = ctx.history(id).rev();
        let now = rev.next()?.close.get();
        let then = rev.nth(self.p.lookback - 1)?.close.get();
        (!then.is_zero()).then(|| now / then - Decimal::ONE)
    }

    /// Signed target units per coin, in `coins` order; `None` (no
    /// decision) while a rankable coin lacks history.
    fn targets(&self, ctx: &Context) -> Option<Vec<(InstrumentId, Decimal)>> {
        let mut ranked = Vec::new();
        for id in &self.p.coins {
            if ctx.tradable(id) && ctx.delisting(id).is_none() {
                ranked.push((id, self.momentum(ctx, id)?));
            }
        }
        // Stable: equal returns keep listed order.
        ranked.sort_by_key(|a| std::cmp::Reverse(a.1));
        let pairs = self.p.k.min(ranked.len() / 2);
        let each = self.p.capital.get() / Decimal::from(2 * self.p.k);
        let longs: Vec<_> = ranked[..pairs].iter().map(|(id, _)| *id).collect();
        let shorts: Vec<_> = ranked[ranked.len() - pairs..]
            .iter()
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::new();
        for id in &self.p.coins {
            let Some(inst) = ctx.instrument(id) else {
                continue;
            };
            let sign = if longs.contains(&id) {
                Decimal::ONE
            } else if shorts.contains(&id) && inst.allows_short() {
                -Decimal::ONE
            } else {
                Decimal::ZERO
            };
            let units = if sign.is_zero() {
                Decimal::ZERO
            } else {
                let close = ctx.last_candle(id)?.close;
                let q = inst.qty_for(Notional::new(each), close).ok();
                match q {
                    Some(q) => sign * inst.round_qty(q).get(),
                    // Too small for the venue: leave this coin as it is.
                    None => continue,
                }
            };
            out.push((id.clone(), units));
        }
        Some(out)
    }
}

/// True when `c` closes on a multiple of `every` bars since the epoch.
fn on_schedule(c: &Candle, every: i64) -> bool {
    let bar = c.interval.as_millis();
    bar > 0 && ((c.close_time.as_millis() + 1) / bar) % every == 0
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: LongShortMomentum::build }
}

impl Strategy for LongShortMomentum {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, &self.p.coins);
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: stopped"
            );
        }
    }

    /// Decisions come from history and positions alone: only the format
    /// version is saved.
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
        if self.stopped
            || !self.p.coins.contains(&c.instrument)
            || !on_schedule(c, self.p.rebalance_bars)
            || !completes(ctx, c, self.p.coins.iter())
            || !portfolio::idle(ctx, &self.p.coins)
        {
            return Vec::new();
        }
        let Some(targets) = self.targets(ctx) else {
            return Vec::new();
        };
        let mut signals = Vec::new();
        for (id, target) in targets {
            // Risk refuses every order on a halted coin: wait for it.
            if ctx.halted(&id) {
                continue;
            }
            let (Some(inst), Some(last)) = (ctx.instrument(&id), ctx.last_candle(&id)) else {
                continue;
            };
            let held = signed::held(inst, ctx.position(&id).qty().get(), last);
            signals.extend(signed::step(inst, last, held, target, self.p.band));
        }
        // Reductions first: they free margin the openings need.
        signals.sort_by_key(|s| !matches!(s, Signal::Reduce { .. }));
        signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{
        Asset, AssetClass, Instrument, InstrumentSpec, OrderKind, Position, Qty, Side, Size,
    };
    use rust_decimal_macros::dec;

    const SYMS: [&str; 4] = ["AAA", "BBB", "CCC", "DDD"];

    fn inst(sym: &str, perp: bool) -> Instrument {
        Instrument::new(InstrumentSpec {
            id: format!("binance:{sym}USDT").parse().unwrap(),
            base: Asset::new(sym).unwrap(),
            class: if perp {
                AssetClass::Perpetual
            } else {
                AssetClass::Spot
            },
            ..btcusdt().into()
        })
        .unwrap()
    }

    fn id(sym: &str) -> InstrumentId {
        format!("binance:{sym}USDT").parse().unwrap()
    }

    /// k = 1 over four coins, lookback 1, capital 1000 → 500 per side.
    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:AAAUSDT", "binance:BBBUSDT", "binance:CCCUSDT", "binance:DDDUSDT"]
               k = 1
               lookback = 1
               capital = "1000"
               band = "0.1"
               fee = "0.0005"
               slippage_bps = "2"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx(perp: bool) -> Context {
        driver::new(
            SYMS.iter().map(|s| inst(s, perp)).collect(),
            Params::default(),
            10,
        )
    }

    /// Bar `i` with these closes, in SYMS order; the signals of the last
    /// candle (the one completing the close time).
    fn bars(s: &mut dyn Strategy, ctx: &mut Context, i: i64, prices: [&str; 4]) -> Vec<Signal> {
        let mut out = Vec::new();
        for (sym, p) in SYMS.iter().zip(prices) {
            let c = Candle::new(bot_core::CandleSpec {
                instrument: id(sym),
                ..candle(i, p).into()
            })
            .unwrap();
            driver::set_now(ctx, c.close_time);
            driver::push_candle(ctx, c.clone());
            out = s.on_candle(ctx, &c);
        }
        out
    }

    fn hold(ctx: &mut Context, sym: &str, qty: Decimal, price: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                id(sym),
                Qty::new(qty),
                Notional::new(qty * price),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn submit(sym: &str, side: Side, q: Decimal) -> Signal {
        Signal::Submit {
            instrument: id(sym),
            side,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }
    }

    fn reduce(sym: &str, side: Side, q: Decimal) -> Signal {
        Signal::Reduce {
            instrument: id(sym),
            side,
            qty: Qty::new(q),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
        let base = params("").parse::<toml::Table>("t").unwrap();
        for (key, value) in [
            ("k", toml::Value::Integer(0)),
            ("k", toml::Value::Integer(3)),
            ("lookback", toml::Value::Integer(0)),
            ("band", toml::Value::String("1".into())),
            ("capital", toml::Value::String("0".into())),
        ] {
            let mut t = base.clone();
            t.insert(key.into(), value.clone());
            assert!(build(NAME, &Params::new(t)).is_err(), "{key} = {value}");
        }
    }

    /// Bar 0 → 1 returns: AAA +10%, BBB +5%, CCC 0, DDD −10%. Long AAA
    /// 500 / 110 = 4.54545 (lot 1e-5), short DDD 500 / 90 = 5.55555; BBB
    /// and CCC flat. One signal per traded coin, on the completing candle.
    #[test]
    fn longs_the_best_and_shorts_the_worst_on_a_perpetual() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut c = ctx(true);
        s.on_start(&mut c);
        assert!(bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]).is_empty());
        assert_eq!(
            bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "90"]),
            [
                submit("AAA", Side::Buy, dec!(4.54545)),
                submit("DDD", Side::Sell, dec!(5.55555)),
            ]
        );
    }

    /// Held long AAA 4.54545, short DDD 5.55555. Bar 2 returns: AAA −20%
    /// (worst), DDD +30% (best), BBB, CCC 0: both flip, so each closes
    /// reduce-only now and nothing opens until it is flat.
    #[test]
    fn a_flip_closes_both_legs_first() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut c = ctx(true);
        s.on_start(&mut c);
        bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]);
        bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "90"]);
        hold(&mut c, "AAA", dec!(4.54545), dec!(110));
        hold(&mut c, "DDD", dec!(-5.55555), dec!(90));
        assert_eq!(
            bars(s.as_mut(), &mut c, 2, ["88", "105", "100", "117"]),
            [
                reduce("AAA", Side::Sell, dec!(4.54545)),
                reduce("DDD", Side::Buy, dec!(5.55555)),
            ]
        );
        // Flat now; bar 3 keeps the ranking (AAA 88 → 80, −9.09%; DDD
        // 117 → 130, +11.1%) and opens the new sides from flat: long DDD
        // 500 / 130 = 3.84615, short AAA 500 / 80 = 6.25.
        hold(&mut c, "AAA", dec!(0), dec!(88));
        hold(&mut c, "DDD", dec!(0), dec!(117));
        assert_eq!(
            bars(s.as_mut(), &mut c, 3, ["80", "105", "100", "130"]),
            [
                submit("AAA", Side::Sell, dec!(6.25)),
                submit("DDD", Side::Buy, dec!(3.84615)),
            ]
        );
    }

    /// Spot can't short: only the long leg trades. A held long within the
    /// band of its new target (500 / 111 = 4.50450 vs 4.54545 held: 0.04 ≤
    /// 0.1 · 4.5) is left alone.
    #[test]
    fn spot_trades_only_the_long_leg_and_drift_stays_inside_the_band() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut c = ctx(false);
        s.on_start(&mut c);
        bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]);
        assert_eq!(
            bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "90"]),
            [submit("AAA", Side::Buy, dec!(4.54545))]
        );
        hold(&mut c, "AAA", dec!(4.54545), dec!(110));
        assert!(bars(s.as_mut(), &mut c, 2, ["111", "105", "100", "90"]).is_empty());
    }

    /// No decision while any order of ours works on any coin.
    #[test]
    fn waits_while_an_order_works() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut c = ctx(true);
        s.on_start(&mut c);
        bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]);
        let o = bot_core::Order::new(bot_core::OrderSpec {
            id: bot_core::OrderId::new("o-1").unwrap(),
            instrument: id("BBB"),
            side: Side::Buy,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
            created_at: bot_core::Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut c, vec![o]);
        assert!(bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "90"]).is_empty());
    }

    /// A delisting coin isn't ranked and is closed: DDD held short and
    /// delisting, the rest AAA +10%, BBB +5%, CCC 0 → long AAA, short CCC,
    /// DDD bought back reduce-only (listed first: reductions go first).
    #[test]
    fn a_delisting_coin_is_closed_not_ranked() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut c = ctx(true);
        s.on_start(&mut c);
        bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]);
        hold(&mut c, "DDD", dec!(-5), dec!(100));
        driver::set_delisting(
            &mut c,
            id("DDD"),
            Some(bot_core::Timestamp::from_millis(i64::MAX / 2)),
        );
        assert_eq!(
            bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "80"]),
            [
                reduce("DDD", Side::Buy, dec!(5)),
                submit("AAA", Side::Buy, dec!(4.54545)),
                submit("CCC", Side::Sell, dec!(5)),
            ]
        );
    }

    /// rebalance_bars 2: bar i closes at (i + 1) bars since the epoch, so
    /// bars 1, 3, … decide and bars 0, 2, … don't. The ranking of bar 1
    /// trades on bar 1 only; bar 2's (a clear new order) waits for bar 3.
    #[test]
    fn decides_only_on_its_schedule() {
        let mut s = build(NAME, &params("rebalance_bars = 2")).unwrap();
        let mut c = ctx(true);
        s.on_start(&mut c);
        bars(s.as_mut(), &mut c, 0, ["100", "100", "100", "100"]);
        assert_eq!(
            bars(s.as_mut(), &mut c, 1, ["110", "105", "100", "90"]),
            [
                submit("AAA", Side::Buy, dec!(4.54545)),
                submit("DDD", Side::Sell, dec!(5.55555)),
            ]
        );
        hold(&mut c, "AAA", dec!(4.54545), dec!(110));
        hold(&mut c, "DDD", dec!(-5.55555), dec!(90));
        assert!(bars(s.as_mut(), &mut c, 2, ["88", "105", "100", "117"]).is_empty());
        // Bar 3: every return 0, a tie, so listed order keeps AAA long
        // and DDD short, resized to the new closes: AAA 500 / 88 =
        // 5.68181 (+1.13636, beyond the band), DDD 500 / 117 = 4.27350
        // (cut by 1.28205, reduce-only, emitted first).
        assert_eq!(
            bars(s.as_mut(), &mut c, 3, ["88", "105", "100", "117"]),
            [
                reduce("DDD", Side::Buy, dec!(1.28205)),
                submit("AAA", Side::Buy, dec!(1.13636)),
            ]
        );
    }
}
