use crate::{BacktestProvider, Report};
use bot_core::{Interval, Notional};
use engine::{EngineConfig, EngineError};
use strategy::{Params, Strategy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs `strategy` over the provider's candles through the shared engine
/// loop and reports the result. `initial_cash` seeds the report's cash
/// ledger; `interval` annualizes Sharpe.
pub async fn run_backtest(
    provider: &BacktestProvider,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
    initial_cash: Notional,
    interval: Interval,
) -> Result<Report, BacktestError> {
    let run = engine::run(provider, strategy, params, config).await?;
    Ok(Report::compute(
        &run,
        provider.candles(),
        provider.instruments(),
        initial_cash,
        interval,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};
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
            RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            BpsSlippage::zero(),
        );
        let params =
            Params::from_toml("instrument = \"binance:BTCUSDT\"\nquote_amount = \"1000\"").unwrap();
        let mut s = strategy::build("buy_and_hold", &params).unwrap();
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
        };
        let r = run_backtest(
            &provider,
            s.as_mut(),
            params,
            &config,
            Notional::new(dec!(1000)),
            Interval::H1,
        )
        .await
        .unwrap();
        assert_eq!(r.fills, 1);
        assert_eq!(r.closed_trades, 0);
        assert_eq!(r.realized_pnl, Notional::ZERO);
        assert_eq!(r.final_equity, Notional::new(dec!(1018.78217803)));
    }
}
