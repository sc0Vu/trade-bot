//! risk_parity over a fixed 10-candle BTC + ETH fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/risk_parity`), 1h flat candles:
//! BTC 100, 110, 99, 101, 100, 120, 96, 100, 101, 100;
//! ETH 10, 10.5, 10, 10.2, 10, 10.1, 10, 9.9, 10, 10.1.
//! vol_period 2 (deliberately short: weights swing, so the path trades
//! both ways), band 0.05, capital 10000 = the broker's cash. Fee 0.1% in
//! the received asset; slippage 10 bps on taker fills, rounded to tick
//! (BTC 0.01, ETH 0.001) against the trader.

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

fn eth() -> InstrumentId {
    "binance:ETHUSDT".parse().unwrap()
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/risk_parity"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc(), eth()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(10 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           vol_period = 2
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc(), eth()],
        history_len: 10,
        ..Default::default()
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("risk_parity", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("risk_parity", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// From a separate Python simulation (decimal, 28 digits, exact square
/// roots) of the fill model, risk and the rule, not from this code.
/// Weights = (1/sd_i) / Σ(1/sd_j), sd over the last 2 hourly returns.
///   t2: first full window → buy both (BTC 0.3244, ETH 0.6756 of 10000),
///       filled at t3's open.
///   t4: BTC's sd collapses (101 → 100 after 99 → 101): it gets more
///       weight → buy BTC, sell ETH (both fill at t5).
///   t5: BTC +20% → its weight falls → sell; t6, t7: weights swing back
///       and forth; t8: BTC calm again → buy BTC, sell ETH.
/// Where a buy's coin sorts before the sale funding it (t5, t9), the buy
/// waits for the sale at the same open and fills after it.
/// Budgets truncate at 8 dp, so the approximate root can't move a fill
/// unless a budget lands within ~1e-20 of a truncation edge.
#[tokio::test]
async fn trades_both_ways_as_relative_volatility_moves() {
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
            row(3, "BTCUSDT", Side::Buy, dec!(101.11), dec!(32.43991)),
            row(3, "ETHUSDT", Side::Buy, dec!(10.211), dec!(658.11379)),
            row(5, "ETHUSDT", Side::Sell, dec!(10.089), dec!(233.60458)),
            row(5, "BTCUSDT", Side::Buy, dec!(120.12), dec!(19.40676)),
            row(6, "BTCUSDT", Side::Sell, dec!(95.90), dec!(40.95795)),
            row(7, "BTCUSDT", Side::Sell, dec!(99.90), dec!(6.28200)),
            row(7, "ETHUSDT", Side::Buy, dec!(9.910), dec!(459.03966)),
            row(8, "BTCUSDT", Side::Sell, dec!(100.89), dec!(4.51712)),
            row(8, "ETHUSDT", Side::Buy, dec!(10.010), dec!(47.54292)),
            row(9, "ETHUSDT", Side::Sell, dec!(10.089), dec!(360.57456)),
            row(9, "BTCUSDT", Side::Buy, dec!(100.10), dec!(35.99087)),
        ]
    );
    let (b, e) = (&r.positions[0], &r.positions[1]);
    assert_eq!(b.qty(), Qty::new(dec!(35.99263246)));
    assert_eq!(e.qty(), Qty::new(dec!(569.35253363)));
    assert_eq!(b.realized_pnl().get().round_dp(12), dec!(-600.889072476307));
    assert_eq!(e.realized_pnl().get().round_dp(12), dec!(-27.277337526119));
}

/// Cash 36.02597268474 + 35.99263246·100 + 569.35253363·10.1 (t9 closes).
#[tokio::test]
async fn report_matches_the_simulated_run() {
    let r = report().await;
    assert_eq!(r.fills, 11);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(9385.74980834774)));
    assert!(r.min_cash >= Notional::ZERO, "never overspent");
}
