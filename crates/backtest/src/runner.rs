use crate::{BacktestProvider, Report};
use bot_core::Interval;
use engine::{EngineConfig, EngineError};
use strategy::{Params, Strategy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs `strategy` over the provider's candles through the shared engine
/// loop and reports the result. The provider's initial cash is both the
/// broker's balance and the report's starting equity; `interval`
/// annualizes Sharpe.
pub async fn run_backtest(
    provider: &BacktestProvider,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
    interval: Interval,
) -> Result<Report, BacktestError> {
    let run = engine::run(provider, strategy, params, config).await?;
    Ok(Report::compute_full(
        &run,
        provider.candles(),
        provider.events(),
        provider.instruments(),
        provider.initial_cash(),
        interval,
        provider.earn(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaperConfig;
    use crate::test_support::{btcusdt, candle};
    use bot_core::Notional;
    use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
    use rust_decimal_macros::dec;

    /// Owner-confirmed engine scenario (see engine tests), now through
    /// BacktestProvider and the report: 1000 USDT budget, fee 0.1% in base,
    /// no slippage -> spend 999.99999, hold 9.89108901 BTC.
    /// Equity at c2 close 103: cash 0.00001 + 9.89108901 * 103
    /// (= 1018.78216803) = 1018.78217803.
    #[tokio::test]
    async fn buy_and_hold_through_backtest_provider() {
        let id = btcusdt().id.clone();
        let provider = BacktestProvider::new(
            vec![btcusdt()],
            vec![
                candle(&id, 0, ["100", "101", "99", "100"]),
                candle(&id, 1, ["101", "103", "100", "102"]),
                candle(&id, 2, ["102", "104", "101", "103"]),
            ],
            PaperConfig {
                fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        );
        let params = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nquote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"0\"",
        )
        .unwrap();
        let mut s = strategy::build("buy_and_hold", &params).unwrap();
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let r = run_backtest(&provider, s.as_mut(), params, &config, Interval::H1)
            .await
            .unwrap();
        assert_eq!(r.fills, 1);
        assert_eq!(r.closed_trades, 0);
        assert_eq!(r.realized_pnl, Notional::ZERO);
        assert_eq!(r.final_equity, Notional::new(dec!(1018.78217803)));
        // 1000 budget, 999.99999 spent: never below zero.
        assert_eq!(r.min_cash, Notional::new(dec!(0.00001)));
    }

    /// Records the strategy's view of realized P&L at each funding event,
    /// after the engine settled it.
    struct Watch {
        inner: Box<dyn Strategy>,
        seen: Vec<Notional>,
    }

    impl Strategy for Watch {
        fn name(&self) -> &str {
            "watch"
        }
        fn on_event(
            &mut self,
            ctx: &mut strategy::Context,
            e: &bot_core::MarketEvent,
        ) -> Vec<bot_core::Signal> {
            if let bot_core::MarketEvent::Funding { instrument, .. } = e {
                self.seen.push(ctx.position(instrument).realized_pnl());
            }
            self.inner.on_event(ctx, e)
        }
        fn on_fill(&mut self, ctx: &mut strategy::Context, f: &bot_core::Fill) {
            self.inner.on_fill(ctx, f)
        }
    }

    /// buy_and_hold on a perpetual, isolated 2x, wallet 1,000, taker 0.1%
    /// in quote: the 1,000 budget buys 10 @ 100 at t1's open (fee 1,
    /// margin 500). Funding 0.1% at mark 110 at t2's open: the long pays
    /// 1.1, and the strategy sees realized −2.1 at that event. Equity at
    /// t2's close 120: 1,000 − 1 − 1.1 + 10 × (120 − 100) = 1,197.9.
    #[tokio::test]
    async fn a_perpetual_backtest_runs_through_the_engine_with_funding() {
        use bot_core::margin::{Bracket, Brackets, MarginMode};
        use bot_core::{AssetClass, Instrument, InstrumentSpec, MarketEvent, Price, Timestamp};
        use exchange::paper::PerpTerms;
        use std::collections::HashMap;
        let perp = Instrument::new(InstrumentSpec {
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let id = perp.id.clone();
        let brackets = Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000)),
            max_leverage: 20,
            rate: dec!(0.004),
            cum: dec!(0),
        }])
        .unwrap();
        let provider = BacktestProvider::new(
            vec![perp],
            vec![
                candle(&id, 0, ["100", "100", "100", "100"]),
                candle(&id, 1, ["100", "110", "100", "110"]),
                candle(&id, 2, ["110", "120", "110", "120"]),
            ],
            PaperConfig {
                fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Quote).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_perp(crate::PerpSetup {
            terms: HashMap::from([(
                id.clone(),
                PerpTerms {
                    leverage: 2,
                    mode: MarginMode::Isolated,
                    brackets,
                },
            )]),
            clearance: dec!(0.0125),
        })
        .with_events(vec![MarketEvent::Funding {
            instrument: id.clone(),
            rate: dec!(0.001),
            mark: Price::new(dec!(110)),
            ts: Timestamp::from_millis(2 * crate::test_support::HOUR),
        }]);
        let params = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nquote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"0\"",
        )
        .unwrap();
        let mut s = Watch {
            inner: strategy::build("buy_and_hold", &params).unwrap(),
            seen: Vec::new(),
        };
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let r = run_backtest(&provider, &mut s, params, &config, Interval::H1)
            .await
            .unwrap();
        assert_eq!(r.fills, 1);
        assert_eq!(s.seen, vec![Notional::new(dec!(-2.1))]);
        assert_eq!(r.funding, Notional::new(dec!(-1.1)));
        assert_eq!(r.final_equity, Notional::new(dec!(1197.9)));
    }
}
