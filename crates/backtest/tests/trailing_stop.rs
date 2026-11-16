//! trailing_stop over a fixed 12-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/trailing_stop`): BTC 1h, flat candles at 100,
//! 104, 110, 115, 112, 106, 102, 98, 95, 97, 100, 103. trail_pct 0.1,
//! reentry_pct 0.05, quote_amount 1000, cash 2000. Fee 0.1% in the
//! received asset; slippage 10 bps on taker fills, rounded to the 0.01
//! tick against the trader.

use backtest::{BacktestProvider, DataDir, PaperConfig, Report, run_backtest};
use bot_core::{InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
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
        "/tests/fixtures/trailing_stop"
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
            initial_cash: Notional::new(dec!(2000)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           quote_amount = "1000"
           trail_pct = "0.1"
           reentry_pct = "0.05"
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
    let mut s = strategy::build("trailing_stop", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("trailing_stop", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// From a separate Python simulation of the fill model, risk and the rule.
///   t0: first candle → buy 1000; fills t1 open 104 → 104.11: 9.60522,
///       fee 0.00960522 BTC → hold 9.59561478.
///   Long from t1 (104): peak 115 at t3; stop 103.5. t6 close 102 → sell
///       the lot-rounded 9.59561 (4.78e-6 dust stays, unsellable = out);
///       fills t7 open 98 → 97.90.
///   Out from t7 (98): trough 95 at t8; re-entry at 99.75. t10 close 100
///       → buy 1000; fills t11 open 103 → 103.11: 9.69838.
/// Realized −61.528147275056: the stop cost 6% plus a round trip of fees
/// and slippage, and the re-entry paid 103.11 for what sold at 97.90.
#[tokio::test]
async fn sells_off_the_peak_and_buys_back_off_the_trough() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<(i64, Side, Price, Qty)> = r
        .fills
        .iter()
        .map(|f| (f.ts.as_millis() / HOUR, f.side, f.price, f.qty))
        .collect();
    let p = |d| Price::new(d);
    let q = |d| Qty::new(d);
    assert_eq!(
        got,
        vec![
            (1, Side::Buy, p(dec!(104.11)), q(dec!(9.60522))),
            (7, Side::Sell, p(dec!(97.90)), q(dec!(9.59561))),
            (11, Side::Buy, p(dec!(103.11)), q(dec!(9.69838))),
        ]
    );
    let pos = &r.positions[0];
    assert_eq!(pos.qty(), Qty::new(dec!(9.6886864)));
    assert_eq!(
        pos.realized_pnl().get().round_dp(12),
        dec!(-61.528147275056)
    );
}

/// Cash 938.471392781 + 9.6886864·103.
#[tokio::test]
async fn report_matches_the_simulated_run() {
    let r = report().await;
    assert_eq!(r.fills, 3);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(0)));
    assert_eq!(r.final_equity, Notional::new(dec!(1936.406091981)));
}
