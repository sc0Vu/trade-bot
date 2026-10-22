//! `buy_and_hold`: spend a fixed quote amount on one instrument at the first
//! chance, then hold forever. Exists to exercise the engine end to end and
//! as a baseline: a strategy that can't beat it after costs has no edge.

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration};
use bot_core::{Candle, InstrumentId, Notional, OrderKind, Side, Signal, Size};
use serde::Deserialize;

const NAME: &str = "buy_and_hold";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyAndHoldParams {
    /// What to buy (`binance:BTCUSDT`).
    instrument: InstrumentId,
    /// Quote to spend (`"1000"` USDT), converted to units by risk at the
    /// last close.
    quote_amount: Notional,
}

/// Buys once with a market order, then never trades again.
///
/// Emits the buy on the first candle of its instrument while flat, and only
/// once: the fill lands a candle later, so "still flat" alone would buy
/// again. If risk rejects the buy, it does not retry.
pub struct BuyAndHold {
    params: BuyAndHoldParams,
    submitted: bool,
}

impl BuyAndHold {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let params: BuyAndHoldParams = params.parse(NAME)?;
        if !params.quote_amount.is_positive() {
            return Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: "quote_amount must be positive".to_owned(),
            });
        }
        Ok(Box::new(Self {
            params,
            submitted: false,
        }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: BuyAndHold::build }
}

impl Strategy for BuyAndHold {
    fn name(&self) -> &str {
        NAME
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let id = &self.params.instrument;
        if self.submitted || c.instrument != *id || !ctx.position(id).is_flat() {
            return Vec::new();
        }
        self.submitted = true;
        vec![Signal::Submit {
            instrument: id.clone(),
            side: Side::Buy,
            size: Size::Quote(self.params.quote_amount),
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
            r#"instrument = "binance:BTCUSDT"
               quote_amount = "1000""#,
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
            r#"instrument = "binance:BTCUSDT"
               quote_amount = 1000.0"#,
        )
        .unwrap();
        assert!(matches!(
            build(NAME, &float),
            Err(StrategyError::Params { .. })
        ));
        let zero = Params::from_toml(
            r#"instrument = "binance:BTCUSDT"
               quote_amount = "0""#,
        )
        .unwrap();
        assert!(build(NAME, &zero).is_err());
        let extra = Params::from_toml(
            r#"instrument = "binance:BTCUSDT"
               quote_amount = "1"
               typo = 1"#,
        )
        .unwrap();
        assert!(build(NAME, &extra).is_err());
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
}
