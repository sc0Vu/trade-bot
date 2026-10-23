//! buy_and_hold, the benchmark, over the same 7-candle BTC fixture as
//! sma_trend (`tests/fixtures/sma_trend`: flat candles at 100, 100, 100,
//! 110, 112, 105, 104), with the same 1009.08 budget and costs, so the two
//! regressions compare a strategy against its baseline on identical data.
//! Fee 0.1% maker and taker in the received asset; slippage 10 bps on taker
//! fills, rounded to the 0.01 tick against the trader.

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
        "/tests/fixtures/sma_trend"
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
            initial_cash: Notional::new(dec!(1009.08)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           quote_amount = "1009.08"
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
    let mut s = strategy::build("buy_and_hold", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("buy_and_hold", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner, 2026-10-23.
///   t0: first candle, flat -> buy, quote 1009.08.
///   t1 open 100 * 1.001 = 100.10: 1009.08 / 100.10 = 10.080719... -> 10.08071
///      BTC (spend 1009.079071), fee 0.01008071 BTC, hold 10.07062929 at
///      cost 1009.079071, cash 0.000929. Never sells.
/// Equity at closes: 1009.08, 1007.063858 x2, 1107.7701509, 1127.91140948,
/// 1057.41700445, 1047.34637516. Max drawdown from the 1127.91140948 peak
/// = 0.071428512597 (12 dp). Sharpe 13.015174.
#[tokio::test]
async fn buys_once_at_the_next_open_and_holds() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 1);
    let buy = &r.fills[0];
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(HOUR), "t1's open");
    assert_eq!(buy.price, Price::new(dec!(100.10)));
    assert_eq!(buy.qty, Qty::new(dec!(10.08071)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.01008071))));

    let pos = &r.positions[0];
    assert_eq!(pos.qty(), Qty::new(dec!(10.07062929)));
    assert_eq!(pos.cost_basis(), Notional::new(dec!(1009.079071)));
    assert_eq!(pos.realized_pnl(), Notional::ZERO);
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 1);
    assert_eq!(r.closed_trades, 0);
    assert_eq!(r.win_rate, None);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(1047.34637516)));
    assert_eq!(r.realized_pnl, Notional::ZERO);
    assert_eq!(r.min_cash, Notional::new(dec!(0.000929)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.071428512597));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(13.015174));
}
