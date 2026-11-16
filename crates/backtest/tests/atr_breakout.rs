//! atr_breakout over a fixed 9-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/atr_breakout`): BTC 1h, (open, high, low, close)
//!   t0-t3  100 101  99 100   (quiet: true range 2)
//!   t4     100 107 100 106
//!   t5     106 110 105 109
//!   t6     109 116 108 115
//!   t7     115 115 107 108
//!   t8     108 109 105 106
//! period 3, entry_mult 0.5, exit_mult 1. Fee 0.1% maker and taker in the
//! received asset; slippage 10 bps on taker fills, rounded to the 0.01 tick
//! against the trader.

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
        "/tests/fixtures/atr_breakout"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(9 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(954.99)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           period = 3
           entry_mult = "0.5"
           exit_mult = "1"
           quote_amount = "954.99"
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
    let mut s = strategy::build("atr_breakout", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("atr_breakout", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits). Over the last 3 bars:
/// entry = mean close + 0.5·ATR, stop = highest high − 1·ATR, ATR = mean of
/// 3 true ranges (TR = max(high−low, |high−prev close|, |low−prev close|)).
///   t0-t2: fewer than 4 candles, nothing. t3: ATR 2, entry 101 > 100.
///   t4: TR 7, ATR (2+2+7)/3 = 11/3, mean (100+100+106)/3 = 102, entry
///      103.83 < 106 -> buy 954.99.
///   t5 open 106 * 1.001 = 106.106 -> 106.11 (tick up): 954.99 / 106.11 =
///      9 BTC exactly, fee 0.009 BTC, hold 8.991 at cost 954.99, cash 0.
///   t5: TR max(5, 4, 1) = 5, ATR 14/3, peak 110, stop 105.33 < 109: hold.
///   t6: TR max(8, 7, 1) = 8, ATR 20/3, peak 116, stop 109.33 < 115: hold.
///   t7: TR max(8, 0, 8) = 8, ATR 7, peak 116, stop 109 > 108 -> sell 8.991.
///   t8 open 108 * 0.999 = 107.892 -> 107.89 (tick down): 8.991 * 107.89 =
///      970.03899, fee 0.97003899 USDT, proceeds 969.06895101, realized
///      +14.07895101; flat.
///   t8: TR max(4, 1, 3) = 4, ATR 20/3, mean 329/3, entry 113 > 106.
/// Equity at closes: 954.99 x5, 980.019, 1033.965, 971.028, 969.06895101.
/// Max drawdown (1033.965 - 969.06895101) / 1033.965 = 0.062764260870
/// (12 dp). Sharpe 6.616311.
#[tokio::test]
async fn rides_the_breakout_until_the_chandelier_stop() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);

    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(5 * HOUR), "t5's open");
    assert_eq!(buy.price, Price::new(dec!(106.11)));
    assert_eq!(buy.qty, Qty::new(dec!(9)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.009))));

    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(8 * HOUR), "t8's open");
    assert_eq!(sell.price, Price::new(dec!(107.89)));
    assert_eq!(sell.qty, Qty::new(dec!(8.991)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.97003899))));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(14.07895101)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(1)));
    assert_eq!(r.rejected_signals, 0);
    // Cross-check by cash flow: 954.99 spent, 969.06895101 received.
    assert_eq!(r.final_equity, Notional::new(dec!(969.06895101)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(14.07895101)));
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.062764260870));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(6.616311));
}
