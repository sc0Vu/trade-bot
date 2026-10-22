//! ratio_hop over a fixed 7-candle BTC/ETH fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/ratio_hop`): BTC flat at 100; ETH closes
//! 10, 10, 8, 8, 10, 10, 10 (t2 dips to 8, t4 opens 8 and closes 10).
//! Fee 0.1% maker and taker, in the received asset. Slippage 10 bps on
//! taker fills, rounded to the 0.01 tick against the trader, and the same
//! 10 bps in the strategy's estimate: no fill happens at a price the
//! decision saw.

use backtest::{BacktestProvider, DataDir, Report, run_backtest};
use bot_core::{InstrumentId, Interval, Notional, Timestamp};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const SLIPPAGE_BPS: &str = "10";

fn ids() -> Vec<InstrumentId> {
    vec![
        "binance:BTCUSDT".parse().unwrap(),
        "binance:ETHUSDT".parse().unwrap(),
    ]
}

async fn run(freefall_pct: &str) -> Report {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ratio_hop"
    ));
    let provider = BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(7 * 3_600_000),
        RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        BpsSlippage::new(SLIPPAGE_BPS.parse().unwrap()).unwrap(),
    )
    .unwrap();
    let params = Params::from_toml(&format!(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "{SLIPPAGE_BPS}"
           freefall_pct = "{freefall_pct}"
           freefall_candles = 2"#
    ))
    .unwrap();
    let mut s = strategy::build("ratio_hop", &params).unwrap();
    let config = EngineConfig {
        whitelist: ids(),
        history_len: 10,
    };
    run_backtest(
        &provider,
        s.as_mut(),
        params,
        &config,
        Notional::new(dec!(1000)),
        Interval::H1,
    )
    .await
    .unwrap()
}

/// Expected values (confirmed by owner, 2026-10-22), worked by hand.
/// Buy price = open*1.001 rounded up to tick; sell = open*0.999 down.
///   t1 buy BTC, budget 1000 @ 100.10: 9.99 BTC (spend 999.999 <= 1000),
///      fee 0.00999 BTC, hold 9.98001, cash 0.001
///   t1 baselines: BTC 9.98001; ETH = lot(9.98001*99.90*0.999/10.01)*0.999
///      = 99.40158891
///   t2 ETH 8: est = lot(9.98001*99.90*0.999/8.01)*0.999 = 124.22096469 -> sell
///   t3 sell 9.98001 BTC @ 99.90: 997.002999 - 0.997002999 fee
///      = 996.005996001; BTC realized 996.005996001 - 999.999 = -3.993003999
///   t4 buy ETH, budget 996.005996001 @ 8.01: 124.34531 ETH (spend
///      996.0059331), hold 124.22096469; ETH 10: est BTC 12.37248513 > 9.98001
///      -> sell 124.22096
///   t5 sell @ 9.99: 1240.9673904 - 1.2409673904 = 1239.7264230096;
///      ETH realized = 1239.7264230096 - 996.0059331*124.22096/124.22096469
///      = 243.7205275141...
///   t6 buy BTC, budget 1239.7264230096 @ 100.10: 12.38487 BTC (spend
///      1239.725487), hold 12.37248513; est ETH 123.23076588 <= 124.22096 -> hold
/// Equity at closes: 1000, 998.002, 998.002, 996.006996001, 1242.210709801,
/// 1239.7275328106, 1237.2505588106. Max drawdown 0.003993003999 (t3).
/// Sharpe 36.735515 (Python decimal check, 28 digits).
#[tokio::test]
async fn rotates_twice_and_ratchets_units() {
    let r = run("0.5").await;
    assert_eq!(r.fills, 5);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.win_rate, Some(dec!(0.5)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(1237.2505588106)));
    assert_eq!(r.realized_pnl.get().round_dp(10), dec!(239.7275235151));
    // Cash after each fill: 0.001, 996.006996001, 0.001062901,
    // 1239.7274859106, 0.0019989106. Never spends quote it doesn't have.
    assert_eq!(r.min_cash, Notional::new(dec!(0.001)));
    assert_eq!(r.max_drawdown, dec!(0.003993003999));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(36.735515));
}

/// With freefall_pct 10%, ETH's 20% drop (10 -> 8 over 2 candles) blocks
/// the t2 hop; by t4 ETH is back at the baseline ratio, so it never
/// rotates. One fill; equity = cash 0.001 + 9.98001 BTC * 100 = 998.002.
#[tokio::test]
async fn freefall_guard_blocks_rotation_into_falling_coin() {
    let r = run("0.1").await;
    assert_eq!(r.fills, 1);
    assert_eq!(r.closed_trades, 0);
    assert_eq!(r.win_rate, None);
    assert_eq!(r.final_equity, Notional::new(dec!(998.002)));
    assert_eq!(r.min_cash, Notional::new(dec!(0.001)));
    assert_eq!(r.max_drawdown, dec!(0.001998));
}
