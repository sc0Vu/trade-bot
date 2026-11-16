//! vol_target over a fixed 12-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/vol_target`): BTC 1h, flat candles at
//! 100 x4, 101, 99, 101, 99, 100 x4 — calm, a burst of ±1-2% swings, calm.
//! target_vol 0.8, vol_period 3, max_weight 0.9, band 0.05, capital 2000 =
//! the broker's cash. Fee 0.1% maker and taker in the received asset;
//! slippage 10 bps on taker fills, rounded to the 0.01 tick against the
//! trader.

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
        "/tests/fixtures/vol_target"
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
           capital = "2000"
           target_vol = "0.8"
           vol_period = 3
           max_weight = "0.9"
           band = "0.05"
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
    let mut s = strategy::build("vol_target", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("vol_target", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits, exact square roots; a
/// separate simulation of the fill model and the rule). Weight =
/// min(0.9, 0.8 / (sd of the last 3 hourly returns · √8760)).
///   t3: no movement → 0.9: buy 1800. t4 open 101 → 101.11: 17.80239 BTC.
///   t5 (sd 0.0152, vol 1.42): weight 0.5635 vs held 0.898 → sell 6.62364.
///   t6 (vol 1.95): 0.4112 vs 0.565 → sell 3.03942.
///   t7, t8: 0.370, 0.411 vs held ≈ 0.41: in band.
///   t9 (vol 1.42): 0.5619 vs 0.410 → buy 300.45066916 (8 dp).
///   t10 (vol 0.55): 0.9 vs 0.562 → buy 669.49485422.
/// Fills: buy 17.80239 @ 101.11 (t4), sell 6.62364 @ 100.89 (t6), sell
/// 3.03942 @ 98.90 (t7), buy 3.0015 @ 100.10 (t10), buy 6.68826 @ 100.10
/// (t11). Hold 17.80159785; cash 197.9441910224; final equity at 100 =
/// 1978.1039760224. Realized −10.12118668420661 (two trims at a loss:
/// swings this short only cost fees and slippage). Max drawdown
/// 0.019662739755, Sharpe −10.337742. Budgets at 8 dp are robust to the
/// approximate root: they'd change only if a weight landed within 1e-11
/// of a rounding edge.
#[tokio::test]
async fn sizes_down_in_the_burst_and_back_up_after() {
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
            (4, Side::Buy, p(dec!(101.11)), q(dec!(17.80239))),
            (6, Side::Sell, p(dec!(100.89)), q(dec!(6.62364))),
            (7, Side::Sell, p(dec!(98.90)), q(dec!(3.03942))),
            (10, Side::Buy, p(dec!(100.10)), q(dec!(3.0015))),
            (11, Side::Buy, p(dec!(100.10)), q(dec!(6.68826))),
        ]
    );
    let pos = &r.positions[0];
    assert_eq!(pos.qty(), Qty::new(dec!(17.80159785)));
    assert_eq!(
        pos.realized_pnl().get().round_dp(12),
        dec!(-10.121186684207)
    );
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 5);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(1978.1039760224)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.019662739755));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-10.337742));
}
