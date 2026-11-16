//! macd_cross over a fixed 12-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/macd_cross`): BTC 1h, flat candles at 100 x7,
//! 103, 106, 104, 100, 98 — a short rally that fails. fast 1, slow 2,
//! signal 2 (window 8 closes). Fee 0.1% maker and taker in the received
//! asset; slippage 10 bps on taker fills, rounded to the 0.01 tick against
//! the trader.

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
        "/tests/fixtures/macd_cross"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(12 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1061.1)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           fast_period = 1
           slow_period = 2
           signal_period = 2
           quote_amount = "1061.1"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc()],
        history_len: 10,
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("macd_cross", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("macd_cross", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits; a separate EMA and fill
/// simulation). Over the newest 8 closes: MACD = close − EMA2 (fast 1 is
/// the close itself), signal = EMA2 of MACD, each seeded with a mean.
///   t0-t6: under 8 closes. t7 (103): MACD 1 > signal 0.667 → buy 1061.1.
///   t8 open 106 * 1.001 = 106.106 -> 106.11: 1061.1 / 106.11 = 10 BTC,
///      fee 0.01 BTC, hold 9.99. Close 106: MACD 1.333 > 1.111, hold.
///   t9 (104): MACD −0.222 < signal 0.222 → sell 9.99.
///   t10 open 100 * 0.999 = 99.9 -> 99.90: 9.99 * 99.90 = 998.001, fee
///      0.998001, proceeds 997.002999, realized −64.097001.
///   t10, t11: flat, MACD −1.41 / −1.14 under the signal: nothing.
/// Equity: 1061.1 x8, 1058.94, 1038.96, 997.002999 x2. Max drawdown
/// 0.060406183206, Sharpe −40.602321.
#[tokio::test]
async fn follows_the_macd_over_its_signal() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);
    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(8 * HOUR));
    assert_eq!(buy.price, Price::new(dec!(106.11)));
    assert_eq!(buy.qty, Qty::new(dec!(10)));
    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(10 * HOUR));
    assert_eq!(sell.price, Price::new(dec!(99.90)));
    assert_eq!(sell.qty, Qty::new(dec!(9.99)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.998001))));
    assert_eq!(
        r.positions[0].realized_pnl(),
        Notional::new(dec!(-64.097001))
    );
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(0)));
    assert_eq!(r.final_equity, Notional::new(dec!(997.002999)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.060406183206));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-40.602321));
}
