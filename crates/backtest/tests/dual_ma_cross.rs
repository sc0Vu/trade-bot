//! dual_ma_cross over a fixed 9-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/dual_ma_cross`): BTC 1h, flat candles at 100,
//! 100, 100, 100, 110, 120, 90, 80, 80 — a short rally and a sharp fall.
//! fast_period 2, slow_period 4. Fee 0.1% maker and taker in the received
//! asset; slippage 10 bps on taker fills, rounded to the 0.01 tick against
//! the trader. The run shows the lag the docs describe: bought after the
//! rise, sold after the fall.

use backtest::{BacktestProvider, DataDir, Report, run_backtest};
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
        "/tests/fixtures/dual_ma_cross"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(9 * HOUR),
        RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        BpsSlippage::new(dec!(10)).unwrap(),
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           fast_period = 2
           slow_period = 4
           quote_amount = "1081.08"
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
    let mut s = strategy::build("dual_ma_cross", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("dual_ma_cross", &p).unwrap();
    run_backtest(
        &provider(),
        s.as_mut(),
        p,
        &config(),
        Notional::new(dec!(1081.08)),
        Interval::H1,
    )
    .await
    .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner, 2026-10-23.
///   t3: fast 100 = slow 100: nothing (first bar with 4 closes).
///   t4: fast (100+110)/2 = 105 > slow 410/4 = 102.5, flat -> buy 1081.08.
///   t5 open 120 * 1.001 = 120.12: 1081.08 / 120.12 = 9 BTC exactly, fee
///      0.009 BTC, hold 8.991 at cost 1081.08, cash 0.
///   t5: fast 115 > slow 107.5: hold. t6: fast 105 = slow 105: nothing.
///   t7: fast 85 < slow 100 -> sell 8.991.
///   t8 open 80 * 0.999 = 79.92: 8.991 * 79.92 = 718.56072, fee 0.71856072
///      USDT, proceeds 717.84215928, realized -363.23784072; flat.
///   t8: fast 80 < slow 92.5, flat: nothing.
/// Equity at closes: 1081.08 x5, 1078.92, 809.19, 719.28, 717.84215928.
/// Max drawdown 363.23784072 / 1081.08 = 0.335995338661 (12 dp). Sharpe
/// -46.849598.
#[tokio::test]
async fn buys_after_the_rise_and_sells_after_the_fall() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);

    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(5 * HOUR), "t5's open");
    assert_eq!(buy.price, Price::new(dec!(120.12)));
    assert_eq!(buy.qty, Qty::new(dec!(9)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.009))));

    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(8 * HOUR), "t8's open");
    assert_eq!(sell.price, Price::new(dec!(79.92)));
    assert_eq!(sell.qty, Qty::new(dec!(8.991)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.71856072))));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(-363.23784072)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(0)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(717.84215928)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(-363.23784072)));
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.335995338661));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-46.849598));
}
