//! rebalance over a fixed 8-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/rebalance`): BTC 1h, flat candles at 100, 100,
//! 130, 130, 90, 90, 100, 100 — a rise and a fall back past the start.
//! Weights BTC 0.5 (the rest cash), capital 2000 = the broker's cash, band
//! 0.05. Fee 0.1% maker and taker in the received asset; slippage 10 bps on
//! taker fills, rounded to the 0.01 tick against the trader.

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
        "/tests/fixtures/rebalance"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(8 * HOUR),
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
        r#"weights = { "binance:BTCUSDT" = "0.5" }
           capital = "2000"
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
    let mut s = strategy::build("rebalance", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("rebalance", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits; a separate simulation of the
/// fill model and the rule). Own cash = 2000 − (basis − realized).
///   t0: flat, BTC weight 0 → buy 1000.
///   t1 open 100 * 1.001 = 100.10: 1000 / 100.1 → 9.99 BTC (lot), notional
///      999.999, fee 0.00999 BTC; hold 9.98001, basis 999.999.
///      Close 100: weight 998.001 / 1998.002 = 0.4995, in band.
///   t2 close 130: 1297.4013 / 2297.4023 = 0.5647 > 0.55 → sell back to
///      1148.70115: 148.70015 / 130 → 1.14384 BTC.
///   t3 open 130 * 0.999 = 129.87: 1.14384 * 129.87 = 148.5505008, fee
///      0.1485505008, proceeds 148.4019502992; basis share 999.999 *
///      1.14384 / 9.98001 → realized +33.78895330220…
///   t4 close 90: hold 8.83617, 795.2553 / 1943.6582502992 = 0.4092 < 0.45
///      → buy 1943.6582502992 / 2 − 795.2553 = 176.5738251496 → 176.57382514
///      (8 dp, toward zero).
///   t5 open 90 * 1.001 = 90.09: → 1.95997 BTC, fee 0.00195997; hold
///      10.79418003. t6, t7 close 100: weight 0.526, in band.
/// Final: cash 971.8292529992 + 10.79418003 * 100 = 2051.2472559992.
/// Equity at closes: 2000, 1998.002, 2297.4023, 2297.1050502992,
/// 1943.6582502992, 1943.3054556992, 2051.2472559992 (x2). Max drawdown
/// 0.154129228608 (12 dp), Sharpe 7.441173. Buying the same 1000 of BTC
/// at t1 and holding ends at 9.98001 * 100 + 1000.001 = 1998.002: back at
/// the starting price, the trim at 130 and the add at 90 made the 53.
#[tokio::test]
async fn sells_the_rise_and_buys_the_fall() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 3);

    let (entry, trim, add) = (&r.fills[0], &r.fills[1], &r.fills[2]);
    assert_eq!(entry.side, Side::Buy);
    assert_eq!(entry.ts, Timestamp::from_millis(HOUR));
    assert_eq!(entry.price, Price::new(dec!(100.10)));
    assert_eq!(entry.qty, Qty::new(dec!(9.99)));
    assert_eq!(entry.fee, Fee::Base(Qty::new(dec!(0.00999))));

    assert_eq!(trim.side, Side::Sell);
    assert_eq!(trim.ts, Timestamp::from_millis(3 * HOUR));
    assert_eq!(trim.price, Price::new(dec!(129.87)));
    assert_eq!(trim.qty, Qty::new(dec!(1.14384)));
    assert_eq!(trim.fee, Fee::Quote(Notional::new(dec!(0.1485505008))));

    assert_eq!(add.side, Side::Buy);
    assert_eq!(add.ts, Timestamp::from_millis(5 * HOUR));
    assert_eq!(add.price, Price::new(dec!(90.09)));
    assert_eq!(add.qty, Qty::new(dec!(1.95997)));

    let pos = &r.positions[0];
    assert_eq!(pos.qty(), Qty::new(dec!(10.79418003)));
    assert_eq!(pos.realized_pnl().get().round_dp(12), dec!(33.788953302203));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 3);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(2051.2472559992)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.154129228608));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(7.441173));
}
