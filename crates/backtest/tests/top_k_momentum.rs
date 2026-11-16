//! top_k_momentum over a fixed 12-candle BTC + ETH + SOL fixture, through
//! the cache, BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/top_k_momentum`), 1h candles each opening at
//! the previous close (24/7 market), closes:
//! BTC 100, 101, 102, 104, 103, 101, 100, 99, 100, 102, 104, 105;
//! ETH 10, 10.2, 10.5, 10.4, 10.1, 9.9, 10, 10.3, 10.6, 10.8, 10.7, 10.6;
//! SOL 50, 49, 48, 49, 51, 53, 55, 56, 55, 53, 52, 51.
//! Leadership rotates. k 2, lookback 2, band 0.05, capital 9000 = the
//! broker's cash. Fee 0.1% in the received asset; slippage 10 bps on taker
//! fills, rounded to tick against the trader.

use backtest::{BacktestProvider, DataDir, PaperConfig, Report, run_backtest};
use bot_core::{InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn ids() -> Vec<InstrumentId> {
    ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect()
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/top_k_momentum"
    ));
    BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(12 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(9000)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
           k = 2
           lookback = 2
           capital = "9000"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: ids(),
        history_len: 10,
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("top_k_momentum", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("top_k_momentum", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// From a separate Python simulation of the fill model, risk and the rule.
/// Held (decided at tN, filled at tN+1's open):
///   t2 BTC + ETH; t4 BTC + SOL (ETH out); t5 SOL only (BTC's return
///   turned negative: its slot stays cash); t7 ETH + SOL; t8 ETH only;
///   t9 BTC + ETH; t11 BTC only (decided on the last bar: never fills).
/// Each rotation sells first and funds the buy from the estimated
/// proceeds; when both fill at one open the buy may come first (coin
/// order), which the broker allows.
#[tokio::test]
async fn holds_the_two_strongest_and_rotates_as_leadership_moves() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<(i64, String, Side, Price, Qty)> = r
        .fills
        .iter()
        .map(|f| {
            (
                f.ts.as_millis() / HOUR,
                f.instrument.symbol().to_string(),
                f.side,
                f.price,
                f.qty,
            )
        })
        .collect();
    let row = |t, sym: &str, side, p, q| (t, sym.to_owned(), side, Price::new(p), Qty::new(q));
    assert_eq!(
        got,
        vec![
            row(3, "BTCUSDT", Side::Buy, dec!(102.11), dec!(44.07012)),
            row(3, "ETHUSDT", Side::Buy, dec!(10.511), dec!(428.12291)),
            row(5, "BTCUSDT", Side::Sell, dec!(102.89), dec!(1.04352)),
            row(5, "ETHUSDT", Side::Sell, dec!(10.089), dec!(427.69478)),
            row(5, "SOLUSDT", Side::Buy, dec!(51.06), dec!(86.523)),
            row(6, "BTCUSDT", Side::Sell, dec!(100.89), dec!(42.98252)),
            row(6, "SOLUSDT", Side::Sell, dec!(52.94), dec!(2.262)),
            row(8, "ETHUSDT", Side::Buy, dec!(10.311), dec!(444.43187)),
            row(8, "SOLUSDT", Side::Sell, dec!(55.94), dec!(2.338)),
            row(9, "ETHUSDT", Side::Sell, dec!(10.589), dec!(9.68139)),
            row(9, "SOLUSDT", Side::Sell, dec!(54.94), dec!(81.836)),
            row(10, "BTCUSDT", Side::Buy, dec!(102.11), dec!(45.46214)),
            row(10, "ETHUSDT", Side::Sell, dec!(10.789), dec!(4.46654)),
        ]
    );
    let q: Vec<Qty> = r.positions.iter().map(|p| p.qty()).collect();
    assert_eq!(
        q,
        vec![
            Qty::new(dec!(45.41668774)),
            Qty::new(dec!(429.83951522)),
            Qty::new(dec!(0.000477)),
        ]
    );
    let realized: Vec<_> = r
        .positions
        .iter()
        .map(|p| p.realized_pnl().get().round_dp(12))
        .collect();
    assert_eq!(
        realized,
        vec![
            dec!(-60.568601958943),
            dec!(-184.772508203395),
            dec!(324.021232160000),
        ]
    );
}

/// Cash 0.00386218021 + holdings at the t11 closes (105, 10.6, 51).
#[tokio::test]
async fn report_matches_the_simulated_run() {
    let r = report().await;
    assert_eq!(r.fills, 13);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(9325.07926321221)));
    assert!(r.min_cash >= Notional::ZERO);
}
