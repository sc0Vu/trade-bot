//! `rebalance`: hold fixed weights of a capital and trade back to them when
//! they drift.
//!
//! `weights` maps each coin to the fraction of the portfolio it should be
//! (`"0.5"`); what they leave of 1 stays in the quote asset as cash. Value
//! is the strategy's own cash (`capital` less net quote spent on its coins,
//! derived from the books: see `portfolio`) plus each coin at its last
//! close. Once per close time, after every coin's candle has arrived and
//! with no order working on any of them: if a weight has drifted more than
//! `band` from its target, every coin is traded back to its target with
//! market orders, sells first. The first evaluation buys the targets.
//!
//! Why it can earn without predicting direction: rebalancing sells some of
//! what rose and buys some of what fell. When prices oscillate and return,
//! each round trip buys low and sells high ("volatility harvesting"); the
//! gain grows with volatility and with how little the weights are
//! correlated. What it costs: in a one-way trend it keeps selling the
//! winner, so it trails holding; and every rebalance pays fees and
//! slippage on the traded amount. `band` is the lever between the two: a
//! wider band trades less (cheaper), a narrower one harvests smaller
//! swings. A 50/50 coin/cash portfolio also halves exposure, so compare it
//! with `buy_and_hold` of the same capital, not of the same exposure.
//! Expected edge is unverified.
//!
//! Stateless: positions (and so cash) and the last closes are all a
//! decision needs, and both are rebuilt on a restart. A refused order
//! (`capital` more than the account holds) spaces out the retries
//! (`portfolio::Backoff`) instead of re-sending it every bar. All coins must share
//! one quote asset, the cash; otherwise it never trades (logged).
//!
//! Reads only closed candles through `Context` (invariants 2, 5), in
//! `Decimal` (invariant 1).

use super::portfolio::{self, Backoff, Target};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Notional, Signal};
use serde::Deserialize;
use std::collections::BTreeMap;

const NAME: &str = "rebalance";

/// An exact decimal written as a string, as a map value.
#[derive(Debug, Deserialize)]
struct Weight(#[serde(deserialize_with = "exact_decimal")] Decimal);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RebalanceParams {
    /// Coin → target fraction of the portfolio (`{ "binance:BTCUSDT" =
    /// "0.5" }`). Each in (0, 1], summing to at most 1; the rest is cash.
    weights: BTreeMap<InstrumentId, Weight>,
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
    /// How far a weight may drift before everything is traded back, in
    /// absolute fraction (`"0.05"` = 5 points: a 50% target trades below
    /// 45% or above 55%). In (0, 1).
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
pub struct Rebalance {
    p: RebalanceParams,
    targets: Vec<Target>,
    /// The coins don't share one quote asset (or one is missing): never
    /// trade.
    stopped: bool,
    backoff: Backoff,
}

impl Rebalance {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: RebalanceParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.weights.is_empty() {
            return bad("weights must name at least one coin");
        }
        if p.weights
            .values()
            .any(|w| w.0 <= Decimal::ZERO || w.0 > Decimal::ONE)
        {
            return bad("each weight must be in (0, 1]");
        }
        if p.weights.values().map(|w| w.0).sum::<Decimal>() > Decimal::ONE {
            return bad("weights must sum to at most 1");
        }
        if !p.capital.is_positive() {
            return bad("capital must be positive");
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
        let targets = p
            .weights
            .iter()
            .map(|(coin, w)| Target {
                coin: coin.clone(),
                weight: w.0,
            })
            .collect();
        Ok(Box::new(Self {
            p,
            targets,
            stopped: false,
            backoff: Backoff::default(),
        }))
    }

    fn cost(&self) -> Decimal {
        self.p.fee + self.p.slippage_bps / Decimal::from(10_000)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Rebalance::build }
}

/// One quote asset across `coins`, all resolvable: else "cash" means two
/// things. Shared with `vol_target`.
pub(crate) fn one_quote<'a>(
    ctx: &Context,
    coins: impl IntoIterator<Item = &'a InstrumentId>,
) -> bool {
    let quotes: Option<Vec<_>> = coins
        .into_iter()
        .map(|id| ctx.instrument(id).map(|i| i.quote.clone()))
        .collect();
    quotes.is_some_and(|q| q.windows(2).all(|w| w[0] == w[1]))
}

/// True on the candle that completes close time `c.close_time` for every
/// coin in `coins` still trading (the last of them to arrive). A coin past
/// its delisting, or halted, sends no candles, so it isn't waited for:
/// waiting would freeze every multi-coin strategy with it in its list.
/// Shared by the multi-coin strategies.
pub(crate) fn completes<'a>(
    ctx: &Context,
    c: &Candle,
    mut coins: impl Iterator<Item = &'a InstrumentId>,
) -> bool {
    coins.all(|id| {
        !ctx.tradable(id)
            || ctx
                .last_candle(id)
                .is_some_and(|l| l.close_time == c.close_time)
    })
}

impl Strategy for Rebalance {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, self.p.weights.keys());
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: rebalance stopped"
            );
        }
    }

    /// Decisions come from positions and last closes alone, both rebuilt
    /// on a restart: only the format version is saved.
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
        let coins = || self.p.weights.keys();
        if self.stopped
            || !self.p.weights.contains_key(&c.instrument)
            || !completes(ctx, c, coins())
            || coins().any(|id| {
                ctx.open_orders(id).next().is_some() || ctx.cancelling_orders(id).next().is_some()
            })
            || self.backoff.hold()
        {
            return Vec::new();
        }
        portfolio::rebalance(ctx, &self.targets, self.p.capital, self.p.band, self.cost())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::portfolio::tests::{eth_candle, ethusdt, position};
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{
        Asset, Instrument, InstrumentSpec, Order, OrderId, OrderKind, OrderSpec, Qty, Side, Size,
        Timestamp,
    };
    use rust_decimal_macros::dec;

    fn params_with(weights: &str, extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"weights = {{ {weights} }}
               capital = "10000"
               band = "0.05"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn params() -> Params {
        params_with(
            r#""binance:BTCUSDT" = "0.5", "binance:ETHUSDT" = "0.25""#,
            "",
        )
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), ethusdt()], params(), 10)
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, c: Candle) -> Vec<Signal> {
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    fn started() -> (Box<dyn Strategy>, Context) {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        (s, ctx)
    }

    fn buy(id: &Instrument, budget: Decimal) -> Signal {
        Signal::Submit {
            instrument: id.id.clone(),
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
        let bad = |w: &str, extra: &str| build(NAME, &params_with(w, extra)).is_err();
        assert!(bad("", ""), "no coins");
        assert!(bad(r#""binance:BTCUSDT" = "0""#, ""));
        assert!(bad(r#""binance:BTCUSDT" = "1.1""#, ""));
        assert!(bad(
            r#""binance:BTCUSDT" = "0.6", "binance:ETHUSDT" = "0.5""#,
            ""
        ));
        assert!(!bad(
            r#""binance:BTCUSDT" = "0.5", "binance:ETHUSDT" = "0.5""#,
            ""
        ));
        assert!(bad(r#""binance:BTCUSDT" = 0.5"#, ""), "float weight");
        let band = |b: &str| {
            Params::from_toml(&format!(
                r#"weights = {{ "binance:BTCUSDT" = "0.5" }}
                   capital = "10000"
                   band = "{b}"
                   fee = "0.001"
                   slippage_bps = "10""#
            ))
            .unwrap()
        };
        assert!(build(NAME, &band("0")).is_err());
        assert!(build(NAME, &band("1")).is_err());
    }

    /// Waits for both coins' candles of a close time, then buys the
    /// targets from flat: 5000 BTC, 2500 ETH.
    #[test]
    fn decides_once_every_coin_has_closed() {
        let (mut s, mut ctx) = started();
        assert!(feed(s.as_mut(), &mut ctx, candle(0, "100")).is_empty());
        assert_eq!(
            feed(s.as_mut(), &mut ctx, eth_candle(0, "10")),
            vec![buy(&btcusdt(), dec!(5000)), buy(&ethusdt(), dec!(2500))]
        );
    }

    /// Holding the targets at 100/10, cash 2500. BTC to 130: weight 6500 /
    /// 11500 = 0.565, out of the 0.05 band → BTC sells back to 5750
    /// (750/130 → 5.76923 lot), ETH buys 2875 − 2500 = 375.
    #[test]
    fn trades_back_after_drift() {
        let (mut s, mut ctx) = started();
        driver::set_position(
            &mut ctx,
            position(&btcusdt().id, dec!(50), dec!(5000), dec!(0)),
        );
        driver::set_position(
            &mut ctx,
            position(&ethusdt().id, dec!(250), dec!(2500), dec!(0)),
        );
        feed(s.as_mut(), &mut ctx, candle(0, "110"));
        assert!(
            feed(s.as_mut(), &mut ctx, eth_candle(0, "10")).is_empty(),
            "in band"
        );
        feed(s.as_mut(), &mut ctx, candle(1, "130"));
        assert_eq!(
            feed(s.as_mut(), &mut ctx, eth_candle(1, "10")),
            vec![
                Signal::Submit {
                    instrument: btcusdt().id.clone(),
                    side: Side::Sell,
                    size: Size::Base(Qty::new(dec!(5.76923))),
                    kind: OrderKind::Market,
                },
                buy(&ethusdt(), dec!(375)),
            ]
        );
    }

    /// The demo run's loop: capital beyond the account, every buy refused
    /// (`-2010`). After the first refusal one evaluation is skipped, after
    /// the second three; a fill ends the backoff.
    #[test]
    fn backs_off_after_refusals_until_a_fill() {
        let (mut s, mut ctx) = started();
        let eval = |s: &mut dyn Strategy, ctx: &mut Context, i: i64| {
            feed(s, ctx, candle(i, "100"));
            feed(s, ctx, eth_candle(i, "10"))
        };
        let refuse = |s: &mut dyn Strategy, ctx: &mut Context, out: &[Signal]| {
            s.on_rejected(ctx, &out[0], "venue error -2010");
        };
        let out = eval(s.as_mut(), &mut ctx, 0);
        assert_eq!(out.len(), 2, "buys both");
        refuse(s.as_mut(), &mut ctx, &out);
        refuse(s.as_mut(), &mut ctx, &out); // same evaluation: counts once
        assert!(eval(s.as_mut(), &mut ctx, 1).is_empty(), "skip 1 of 1");
        let out = eval(s.as_mut(), &mut ctx, 2);
        assert_eq!(out.len(), 2, "retried");
        refuse(s.as_mut(), &mut ctx, &out);
        for i in 3..6 {
            assert!(eval(s.as_mut(), &mut ctx, i).is_empty(), "skip, bar {i}");
        }
        let out = eval(s.as_mut(), &mut ctx, 6);
        assert_eq!(out.len(), 2, "retried after 3");
        refuse(s.as_mut(), &mut ctx, &out);
        let fill = bot_core::Fill::new(bot_core::FillSpec {
            order_id: OrderId::new("o-9").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            qty: Qty::new(dec!(1)),
            price: "100".parse().unwrap(),
            fee: bot_core::Fee::Base(Qty::new(dec!(0.001))),
            liquidity: bot_core::Liquidity::Taker,
            ts: Timestamp::from_millis(0),
        })
        .unwrap();
        s.on_fill(&mut ctx, &fill);
        assert_eq!(eval(s.as_mut(), &mut ctx, 7).len(), 2, "fill cleared it");
    }

    #[test]
    fn waits_while_any_coin_has_an_order_working() {
        let (mut s, mut ctx) = started();
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: ethusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(2500))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working.clone()]);
        feed(s.as_mut(), &mut ctx, candle(0, "100"));
        assert!(feed(s.as_mut(), &mut ctx, eth_candle(0, "10")).is_empty());
        driver::set_open_orders(&mut ctx, vec![]);
        driver::set_cancelling_orders(&mut ctx, vec![working]);
        feed(s.as_mut(), &mut ctx, candle(1, "100"));
        assert!(feed(s.as_mut(), &mut ctx, eth_candle(1, "10")).is_empty());
    }

    /// BTC/USDT with ETH/BTC: "cash" would be two assets. Never trades.
    #[test]
    fn stops_on_two_quote_assets() {
        let ethbtc = Instrument::new(InstrumentSpec {
            id: "binance:ETHBTC".parse().unwrap(),
            base: Asset::new("ETH").unwrap(),
            quote: Asset::new("BTC").unwrap(),
            ..btcusdt().into()
        })
        .unwrap();
        let p = params_with(
            r#""binance:BTCUSDT" = "0.5", "binance:ETHBTC" = "0.25""#,
            "",
        );
        let mut s = build(NAME, &p).unwrap();
        let ethbtc_bar = Candle::new(bot_core::CandleSpec {
            instrument: ethbtc.id.clone(),
            ..candle(0, "0.05").into()
        })
        .unwrap();
        let mut ctx = driver::new(vec![btcusdt(), ethbtc], p, 10);
        s.on_start(&mut ctx);
        assert!(feed(s.as_mut(), &mut ctx, candle(0, "100")).is_empty());
        assert!(feed(s.as_mut(), &mut ctx, ethbtc_bar).is_empty());
    }

    #[test]
    fn saves_only_its_version() {
        let (mut s, ctx) = started();
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1 }));
        assert!(s.restore_state(state, &ctx).is_ok());
        assert!(
            s.restore_state(serde_json::json!({ "v": 2 }), &ctx)
                .is_err()
        );
    }

    /// BTC stopped trading at 1h: ETH's candle alone completes the close
    /// time after that, instead of waiting for a BTC candle that never
    /// comes. Before 1h, ETH alone doesn't.
    #[test]
    fn a_delisted_coin_is_not_waited_for() {
        let mut ctx = ctx();
        let coins = [btcusdt().id.clone(), ethusdt().id.clone()];
        driver::set_delisting(
            &mut ctx,
            coins[0].clone(),
            Some(Timestamp::from_millis(3_600_000)),
        );
        let early = eth_candle(0, "10");
        driver::set_now(&mut ctx, early.close_time);
        driver::push_candle(&mut ctx, early.clone());
        assert!(!completes(&ctx, &early, coins.iter()));
        let late = eth_candle(1, "10");
        driver::set_now(&mut ctx, late.close_time);
        driver::push_candle(&mut ctx, late.clone());
        assert!(completes(&ctx, &late, coins.iter()));
    }

    /// A halted coin sends no candles: ETH's candle alone completes the
    /// close time while BTC is halted.
    #[test]
    fn a_halted_coin_is_not_waited_for() {
        let mut ctx = ctx();
        let coins = [btcusdt().id.clone(), ethusdt().id.clone()];
        let c = eth_candle(0, "10");
        driver::push_candle(&mut ctx, c.clone());
        assert!(!completes(&ctx, &c, coins.iter()));
        driver::set_trading(&mut ctx, coins[0].clone(), false);
        assert!(completes(&ctx, &c, coins.iter()));
    }
}
