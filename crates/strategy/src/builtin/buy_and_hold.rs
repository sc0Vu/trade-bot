//! `buy_and_hold`: the benchmark. Spend a fixed quote amount on one coin at
//! its first candle, then hold forever.
//!
//! Every other strategy is measured against it: one that can't beat buying
//! and holding the same coin after costs has no edge. It bets only that the
//! coin goes up over the run and pays a single entry; it fails whenever the
//! coin falls, since it never sells and never exits. Kept deliberately
//! minimal so the comparison stays honest.
//!
//! Reads only `Context` and returns a `Signal` (invariant 5); the decision
//! is made on a closed candle (invariant 2); money is `Decimal` (invariant
//! 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "buy_and_hold";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyAndHoldParams {
    /// The one instrument bought (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Quote spent on the single buy (`"1000"` USDT), as a quote-budget
    /// market order: never more than this is spent.
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required, like the other
    /// strategies', so the config states the costs the backtest is run
    /// with; the decision doesn't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage assumed on the market buy, in basis points (`"5"`).
    /// Required and unused in the decision, as `fee`; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs. Decides once, on the coin's first candle: buys if
/// flat, otherwise (a restart with the coin already in the books) holds
/// what is there. Never sells; if the buy is rejected it does not retry.
pub struct BuyAndHold {
    p: BuyAndHoldParams,
    decided: bool,
}

impl BuyAndHold {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: BuyAndHoldParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self { p, decided: false }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: BuyAndHold::build }
}

impl Strategy for BuyAndHold {
    fn name(&self) -> &str {
        NAME
    }

    /// Whether the one decision has been made, so a restart never buys a
    /// second time (even if a rejected buy left the books flat).
    fn save_state(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "v": super::STATE_VERSION, "decided": self.decided }))
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        let decided = state["decided"]
            .as_bool()
            .ok_or_else(|| super::state_error(NAME, "missing decided"))?;
        self.decided = decided;
        Ok(())
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        // The fill lands a candle later, so "still flat" alone would buy
        // again: the decision is made once.
        if self.decided || c.instrument != *coin {
            return Vec::new();
        }
        self.decided = true;
        if !ctx.position(coin).is_flat() {
            return Vec::new();
        }
        vec![Signal::Submit {
            instrument: coin.clone(),
            side: Side::Buy,
            size: Size::Quote(self.p.quote_amount),
            kind: OrderKind::Market,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::MarketEvent;
    use rust_decimal_macros::dec;

    fn params() -> Params {
        Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap()
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    #[test]
    fn unknown_name_errors() {
        assert!(matches!(
            build("nope", &params()),
            Err(StrategyError::Unknown(_))
        ));
    }

    #[test]
    fn bad_params_error_at_build() {
        let float = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = 1000.0
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(matches!(
            build(NAME, &float),
            Err(StrategyError::Params { .. })
        ));
        let zero = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "0"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &zero).is_err());
        let extra = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1"
               fee = "0.001"
               slippage_bps = "10"
               typo = 1"#,
        )
        .unwrap();
        assert!(build(NAME, &extra).is_err());
    }

    #[test]
    fn costs_are_required_and_range_checked() {
        let missing = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1000"
               fee = "0.001""#,
        )
        .unwrap();
        assert!(build(NAME, &missing).is_err(), "slippage_bps is required");
        let negative = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1000"
               fee = "-0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        assert!(build(NAME, &negative).is_err());
    }

    /// No candle of its coin yet: nothing to decide on, nothing emitted.
    #[test]
    fn no_candle_of_its_coin_means_no_signal() {
        let eth = Params::from_toml(
            r#"coin = "binance:ETHUSDT"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
        )
        .unwrap();
        let mut s = build(NAME, &eth).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 10);
        for i in 0..3 {
            let e = MarketEvent::Candle(candle(i, "100"));
            assert!(s.on_event(&mut ctx, &e).is_empty());
        }
    }

    /// A restart with the coin already held: the first candle decides to
    /// hold what is there, and no later candle buys.
    #[test]
    fn already_holding_never_buys() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 10);
        driver::set_position(
            &mut ctx,
            bot_core::Position::from_parts(
                btcusdt().id.clone(),
                bot_core::Qty::new(dec!(1)),
                Notional::new(dec!(100)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        for i in 0..3 {
            let e = MarketEvent::Candle(candle(i, "100"));
            assert!(s.on_event(&mut ctx, &e).is_empty());
        }
    }

    #[test]
    fn buys_once_then_holds() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 10);
        let mut all = Vec::new();
        for i in 0..3 {
            let e = MarketEvent::Candle(candle(i, "100"));
            all.extend(s.on_event(&mut ctx, &e));
        }
        assert_eq!(
            all,
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Buy,
                size: Size::Quote(Notional::new(dec!(1000))),
                kind: OrderKind::Market,
            }]
        );
    }

    /// Restored as decided, a flat restart does not buy again.
    #[test]
    fn a_decided_restart_never_buys_again() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 10);
        s.on_event(&mut ctx, &MarketEvent::Candle(candle(0, "100")));
        let state = s.save_state().unwrap();
        assert_eq!(state, serde_json::json!({ "v": 1, "decided": true }));

        let mut fresh = build(NAME, &params()).unwrap();
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 10);
        fresh.restore_state(state, &ctx).unwrap();
        assert!(
            fresh
                .on_event(&mut ctx, &MarketEvent::Candle(candle(1, "100")))
                .is_empty()
        );
        assert!(
            fresh
                .restore_state(serde_json::json!({ "v": 1 }), &ctx)
                .is_err()
        );
    }
}
