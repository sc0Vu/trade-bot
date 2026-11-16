//! bollinger_reversion over the rsi_reversion fixture (7 candles), through
//! the cache, BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/rsi_reversion`): BTC 1h, flat candles at 100,
//! 100, 90, 85, 95, 105, 100 — a dip and a bounce. period 3, k 1. Fee 0.1%
//! maker and taker in the received asset; slippage 10 bps on taker fills,
//! rounded to the 0.01 tick against the trader.

use backtest::{BacktestProvider, DataDir, PaperConfig, Report, run_backtest};
use bot_core::{Fee, InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rsi_reversion"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(7 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(850.9)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           period = 3
           k = "1"
           quote_amount = "850.9"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc()],
        history_len: 10,
        ..Default::default()
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("bollinger_reversion", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("bollinger_reversion", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits; a separate simulation).
/// Window = last 3 closes; mid = mean, var = population variance.
///   t2: 100, 100, 90 → mid 96.667, var 22.222; gap 6.667² = 44.444 >
///      22.222 → buy 850.9.
///   t3 open 85 * 1.001 = 85.085 -> 85.09: 850.9 / 85.09 = 10 BTC, fee
///      0.01 BTC, hold 9.99. Close 85 < mid 91.667: hold.
///   t4: 90, 85, 95 → mid 90 < 95 → sell 9.99.
///   t5 open 105 * 0.999 = 104.895 -> 104.89: 9.99 * 104.89 = 1047.8511,
///      fee 1.0478511, proceeds 1046.8032489, realized +195.9032489.
///      Close 105 > mid 95: flat, no buy. t6: mid 100 = close: nothing.
/// Equity: 850.9 x3, 849.15, 949.05, 1046.8032489 x2. Max drawdown
/// 0.002056645904, Sharpe 59.374664.
#[tokio::test]
async fn buys_below_the_band_and_sells_above_the_mean() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);
    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(3 * HOUR));
    assert_eq!(buy.price, Price::new(dec!(85.09)));
    assert_eq!(buy.qty, Qty::new(dec!(10)));
    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(5 * HOUR));
    assert_eq!(sell.price, Price::new(dec!(104.89)));
    assert_eq!(sell.qty, Qty::new(dec!(9.99)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(1.0478511))));
    assert_eq!(
        r.positions[0].realized_pnl(),
        Notional::new(dec!(195.9032489))
    );
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(1)));
    assert_eq!(r.final_equity, Notional::new(dec!(1046.8032489)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.002056645904));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(59.374664));
}
