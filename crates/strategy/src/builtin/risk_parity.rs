//! `risk_parity`: hold several coins weighted by the inverse of their
//! volatility, so each carries about the same share of the portfolio's
//! swings.
//!
//! Each close time, once every coin's candle is in and nothing is working:
//! `sd_i` = [`indicator::return_sd`] of coin i over `vol_period` returns;
//! weight_i = `exposure` · (1/sd_i) / Σ_j (1/sd_j). The rest of `capital`
//! (1 − `exposure`) stays cash. Holdings are then traded back to those
//! weights with market orders whenever any coin is more than `band` off,
//! with the shared `portfolio` code (own cash derived from the books,
//! sells first, their proceeds fund the buys, filter-failing trades left
//! out). The per-bar sd needs no annualizing: only ratios matter.
//!
//! Why: an equal-money portfolio of coins is mostly a bet on its most
//! volatile coin, which swings the value most. Weighting by 1/vol spreads
//! the risk instead. It predicts no returns; it lowers concentration, the
//! main risk in a coin list chosen with hindsight. This is "naive" risk
//! parity: it ignores correlations, which between crypto coins are high,
//! so it balances each coin's own swings, not its share of the
//! portfolio's. What it costs: it holds less of a volatile coin in its
//! rallies, trades whenever relative volatility moves the weights past
//! the band, and like `rebalance` trails holding the winner in a one-way
//! trend. Expected edge is unverified.
//!
//! A coin whose closes didn't move at all over the window has sd 0 and no
//! inverse: no decision is made until it moves (a real market always does).
//! The sd uses `Decimal`'s approximate square root: it sets weights only;
//! orders and cash stay exact (invariant 1). Stateless: positions and
//! history are all a decision needs, both rebuilt on a restart. Reads only
//! closed candles through `Context` (invariants 2, 5). All coins must share
//! one quote asset, else it never trades (logged).

use super::portfolio::{self, Backoff, Target};
use super::rebalance::{completes, one_quote};
use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Fill, InstrumentId, Notional, Signal};
use serde::Deserialize;

const NAME: &str = "risk_parity";

fn one() -> Decimal {
    Decimal::ONE
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskParityParams {
    /// The coins held (`["binance:BTCUSDT", "binance:ETHUSDT"]`), at least
    /// two, one quote asset.
    coins: Vec<InstrumentId>,
    /// Quote the strategy manages (`"10000"` USDT): its starting cash.
    capital: Notional,
    /// Returns each coin's volatility is measured over. Needs
    /// `vol_period + 1` closed candles, so the engine's `history_len` must
    /// be at least that. At least 2.
    vol_period: usize,
    /// Fraction of `capital` held in coins (`"1"`, the default); the rest
    /// stays cash. In (0, 1].
    #[serde(deserialize_with = "exact_decimal", default = "one")]
    exposure: Decimal,
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
pub struct RiskParity {
    p: RiskParityParams,
    stopped: bool,
    backoff: Backoff,
}

impl RiskParity {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: RiskParityParams = params.parse(NAME)?;
        let bad = |reason: String| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason,
            })
        };
        if let Err(e) = portfolio::check_coins(&p.coins, 2) {
            return bad(e);
        }
        if p.vol_period < 2 {
            return bad("vol_period must be >= 2".into());
        }
        if p.exposure <= Decimal::ZERO || p.exposure > Decimal::ONE {
            return bad("exposure must be in (0, 1]".into());
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

    /// Inverse-volatility weights, scaled to `exposure`; `None` until every
    /// coin has `vol_period + 1` closes, or while one hasn't moved.
    fn targets(&self, ctx: &Context) -> Option<Vec<Target>> {
        let inverse: Vec<Decimal> = self
            .p
            .coins
            .iter()
            .map(|id| {
                let sd = indicator::return_sd(ctx.history(id), self.p.vol_period)?;
                (!sd.is_zero()).then(|| Decimal::ONE / sd)
            })
            .collect::<Option<_>>()?;
        let total: Decimal = inverse.iter().sum();
        Some(
            self.p
                .coins
                .iter()
                .zip(inverse)
                .map(|(coin, inv)| Target {
                    coin: coin.clone(),
                    weight: self.p.exposure * inv / total,
                })
                .collect(),
        )
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: RiskParity::build }
}

impl Strategy for RiskParity {
    fn name(&self) -> &str {
        NAME
    }

    fn on_start(&mut self, ctx: &mut Context) {
        self.stopped = !one_quote(ctx, &self.p.coins);
        if self.stopped {
            tracing::warn!(
                strategy = NAME,
                "coins missing or not sharing one quote asset: risk_parity stopped"
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
    use bot_core::{Order, OrderId, OrderKind, OrderSpec, Qty, Side, Size, Timestamp};
    use rust_decimal_macros::dec;

    fn params_with(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               capital = "10000"
               vol_period = 2
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

    /// Feeds bar `i` for both coins as the engine does (BTC then ETH);
    /// returns the signals the ETH candle (the one completing the close
    /// time) produced.
    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, btc: &str, eth: &str) -> Vec<Signal> {
        let mut out = Vec::new();
        for c in [candle(i, btc), eth_candle(i, eth)] {
            driver::set_now(ctx, c.close_time);
            driver::push_candle(ctx, c.clone());
            out = s.on_candle(ctx, &c);
            out.extend(driver::take_signals(ctx));
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
        let bad = |p: &str| build(NAME, &Params::from_toml(p).unwrap()).is_err();
        let base = |coins: &str, extra: &str| {
            format!(
                "coins = {coins}\ncapital = \"10000\"\nvol_period = 2\nband = \"0.05\"\n\
                 fee = \"0.001\"\nslippage_bps = \"10\"\n{extra}"
            )
        };
        assert!(bad(&base(r#"["binance:BTCUSDT"]"#, "")), "one coin");
        assert!(
            bad(&base(r#"["binance:BTCUSDT", "binance:btcusdt"]"#, "")),
            "same coin twice"
        );
        let two = r#"["binance:BTCUSDT", "binance:ETHUSDT"]"#;
        assert!(bad(&base(two, r#"exposure = "0""#)));
        assert!(bad(&base(two, r#"exposure = "1.01""#)));
        assert!(bad(&base(two, "exposure = 0.5")), "float");
        assert!(bad(
            &base(two, "").replace("vol_period = 2", "vol_period = 1")
        ));
        assert!(bad(
            &base(two, "").replace(r#"band = "0.05""#, r#"band = "0""#)
        ));
        assert!(!bad(&base(two, r#"exposure = "0.8""#)));
    }

    /// BTC closes 100, 110, 99: returns +10%, −10% → sd = √0.02 = 0.1414214.
    /// ETH closes 10, 10.5, 10: returns +5%, −4.7619% → sd 0.0690271.
    /// Weight BTC = (1/sd_B) / (1/sd_B + 1/sd_E) = sd_E / (sd_B + sd_E) =
    /// 0.328 (exactly, to 39 digits in Python decimal); ETH 0.672. Exposure
    /// 0.8 → 0.2624 and 0.5376 (compared at 6 dp: the root is approximate).
    #[test]
    fn weights_are_inverse_volatility() {
        let s = RiskParity {
            p: params_with(r#"exposure = "0.8""#).parse(NAME).unwrap(),
            stopped: false,
            backoff: Backoff::default(),
        };
        let mut ctx = ctx();
        for (i, (b, e)) in [("100", "10"), ("110", "10.5")].into_iter().enumerate() {
            driver::push_candle(&mut ctx, candle(i as i64, b));
            driver::push_candle(&mut ctx, eth_candle(i as i64, e));
            assert!(s.targets(&ctx).is_none(), "needs 3 closes");
        }
        driver::push_candle(&mut ctx, candle(2, "99"));
        driver::push_candle(&mut ctx, eth_candle(2, "10"));
        let t = s.targets(&ctx).unwrap();
        assert_eq!(t[0].coin, btcusdt().id);
        assert_eq!(t[0].weight.round_dp(6), dec!(0.2624));
        assert_eq!(t[1].weight.round_dp(6), dec!(0.5376));
        assert!((t[0].weight + t[1].weight - dec!(0.8)).abs() < dec!(1e-20));
    }

    #[test]
    fn a_coin_that_never_moved_blocks_the_decision() {
        let mut s = build(NAME, &params_with("")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        for (i, b) in ["100", "110", "99", "105"].into_iter().enumerate() {
            assert!(bar(s.as_mut(), &mut ctx, i as i64, b, "10").is_empty());
        }
    }

    /// Flat, both coins moving: buys both at their weights of 10000, on the
    /// candle completing the close time only.
    #[test]
    fn buys_the_weights_once_all_coins_are_in() {
        let mut s = build(NAME, &params_with("")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        assert!(bar(s.as_mut(), &mut ctx, 0, "100", "10").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "110", "10.5").is_empty());
        // BTC's candle alone decides nothing.
        let c = candle(2, "99");
        driver::push_candle(&mut ctx, c.clone());
        assert!(s.on_candle(&mut ctx, &c).is_empty());
        let c = eth_candle(2, "10");
        driver::push_candle(&mut ctx, c.clone());
        let got = s.on_candle(&mut ctx, &c);
        let [
            Signal::Submit {
                size: Size::Quote(b),
                ..
            },
            Signal::Submit {
                size: Size::Quote(e),
                ..
            },
        ] = got.as_slice()
        else {
            panic!("expected two buys, got {got:?}");
        };
        // 0.328 and 0.672 of 10000 (approximate root: compared rounded).
        assert_eq!(b.get().round_dp(0), dec!(3280));
        assert_eq!(e.get().round_dp(0), dec!(6720));
        assert_eq!(got[0], buy(&btcusdt().id, b.get()));
        assert!(b.get() + e.get() <= dec!(10000));
    }

    #[test]
    fn waits_while_an_order_is_working_and_when_quotes_differ() {
        let mut s = build(NAME, &params_with("")).unwrap();
        let mut ctx = ctx();
        s.on_start(&mut ctx);
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: ethusdt().id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        driver::set_position(
            &mut ctx,
            position(&ethusdt().id, dec!(1), dec!(10), dec!(0)),
        );
        for (i, (b, e)) in [("100", "10"), ("110", "10.5"), ("99", "10")]
            .into_iter()
            .enumerate()
        {
            assert!(bar(s.as_mut(), &mut ctx, i as i64, b, e).is_empty());
        }

        let usdc = bot_core::Instrument::new(bot_core::InstrumentSpec {
            quote: bot_core::Asset::new("USDC").unwrap(),
            ..ethusdt().into()
        })
        .unwrap();
        let mut ctx = driver::new(vec![btcusdt(), usdc], params_with(""), 10);
        let mut s = build(NAME, &params_with("")).unwrap();
        s.on_start(&mut ctx);
        for (i, (b, e)) in [("100", "10"), ("110", "10.5"), ("99", "10")]
            .into_iter()
            .enumerate()
        {
            assert!(bar(s.as_mut(), &mut ctx, i as i64, b, e).is_empty());
        }
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
