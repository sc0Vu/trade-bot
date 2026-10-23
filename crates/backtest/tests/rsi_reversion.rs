//! rsi_reversion over a fixed 7-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/rsi_reversion`): BTC 1h, flat candles at 100,
//! 100, 90, 85, 95, 105, 100 — a dip and a bounce. rsi_period 2, oversold
//! 30, overbought 70. Fee 0.1% maker and taker in the received asset;
//! slippage 10 bps on taker fills, rounded to the 0.01 tick against the
//! trader.

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
           rsi_period = 2
           oversold = "30"
           overbought = "70"
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
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("rsi_reversion", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("rsi_reversion", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner,
/// 2026-10-23. RSI = 100 * G / (G + L)
/// over the last 2 close-to-close changes.
///   t2: changes 0, -10 -> RSI 0 < 30, flat -> buy, quote 850.9.
///   t3 open 85 * 1.001 = 85.085 -> 85.09 (tick up): 850.9 / 85.09 = 10 BTC
///      exactly, fee 0.01 BTC, hold 9.99 at cost 850.9, cash 0.
///   t3: changes -10, -5 -> RSI 0: long, no sell.
///   t4: changes -5, +10 -> 100 * 10 / 15 = 66.67: not above 70, hold.
///   t5: changes +10, +10 -> RSI 100 > 70 -> sell 9.99.
///   t6 open 100 * 0.999 = 99.90: 9.99 * 99.90 = 998.001, fee 0.998001
///      USDT, proceeds 997.002999, realized 146.102999; flat.
///   t6: changes +10, -5 -> 66.67, flat and not oversold: nothing.
/// Equity at closes: 850.9 x3, 849.15, 949.05, 1048.95, 997.002999. Max
/// drawdown from the 1048.95 peak = 51.947001 / 1048.95 = 0.049522857143
/// (12 dp). Sharpe 39.853769.
#[tokio::test]
async fn buys_the_dip_and_sells_the_bounce() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);

    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(3 * HOUR), "t3's open");
    assert_eq!(buy.price, Price::new(dec!(85.09)));
    assert_eq!(buy.qty, Qty::new(dec!(10)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.01))));

    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(6 * HOUR), "t6's open");
    assert_eq!(sell.price, Price::new(dec!(99.90)));
    assert_eq!(sell.qty, Qty::new(dec!(9.99)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.998001))));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(146.102999)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(1)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(997.002999)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(146.102999)));
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.049522857143));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(39.853769));
}
