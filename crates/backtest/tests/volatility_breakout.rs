//! volatility_breakout over a fixed 8-candle BTC fixture, through the
//! cache, BacktestProvider, the engine and the report: the first built-in
//! that trades with stop orders, so this also pins the stop path (trigger
//! inside the bar, gap over the stop, cancel and replace).
//!
//! Fixture (`tests/fixtures/volatility_breakout`): BTC 1h, (open, high,
//! low, close)
//!   t0 100 102  98 100    t4 103 104  98  98
//!   t1 100 101  99 100    t5 102 107 102 106
//!   t2 100 104  99 103    t6 101 102  99 100
//!   t3 104 105 102 103    t7 100 101  95  96
//! k 0.5, quote_amount 1010, cash 2000. Fee 0.1% maker and taker in the
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
        "/tests/fixtures/volatility_breakout"
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
        r#"coin = "binance:BTCUSDT"
           k = "0.5"
           quote_amount = "1010"
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
    let mut s = strategy::build("volatility_breakout", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("volatility_breakout", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits). Flat at a close: buy stop
/// at close + 0.5·(high − low), sized 1010 / stop; long: sell all at market.
///   t0: stop 100 + 2 = 102. t1: high 101 < 102, no fill; cancel, stop
///      100 + 1 = 101.
///   t2: high 104 >= 101, open 100 below it -> fills at the stop:
///      101 * 1.001 = 101.101 -> 101.11 (tick up); 1010 / 101 = 10 BTC,
///      fee 0.01 BTC, hold 9.99, cost 1011.1, cash 988.9. Close: sell 9.99.
///   t3 open 104 * 0.999 = 103.896 -> 103.89: 9.99 * 103.89 = 1037.8611,
///      fee 1.0378611, proceeds 1036.8232389, realized +25.7232389.
///      Close: stop 103 + 1.5 = 104.5.
///   t4: high 104 < 104.5, no fill; cancel, stop 98 + 3 = 101.
///   t5: opens at 102, over the stop -> fills at the open: 102 * 1.001 =
///      102.102 -> 102.11; 10 BTC (sized at the stop), fee 0.01, hold 9.99,
///      cost 1021.1. Close: sell 9.99.
///   t6 open 101 * 0.999 = 100.899 -> 100.89: 9.99 * 100.89 = 1007.8911,
///      fee 1.0078911, proceeds 1006.8832089, realized -14.2167911.
///      Close: stop 100 + 1.5 = 101.5. t7: high 101, no fill; stop 99.
/// Realized 25.7232389 - 14.2167911 = 11.5064478 = final cash 2011.5064478
/// - 2000 (cross-check by cash flow). Equity at closes: 2000, 2000,
/// 2017.87, 2025.7232389, 2025.7232389, 2063.5632389, 2011.5064478,
/// 2011.5064478. Max drawdown (2063.5632389 - 2011.5064478) / 2063.5632389
/// = 0.025226651705 (12 dp). Sharpe 6.274065.
#[tokio::test]
async fn buys_through_the_stop_and_sells_on_the_close() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 4);

    let in_bar = &r.fills[0];
    assert_eq!(in_bar.side, Side::Buy);
    // Inside the bar: when is unknowable, so the fill carries t2's close
    // time, not its open.
    assert_eq!(in_bar.ts, Timestamp::from_millis(3 * HOUR - 1), "during t2");
    assert_eq!(in_bar.price, Price::new(dec!(101.11)));
    assert_eq!(in_bar.qty, Qty::new(dec!(10)));
    assert_eq!(in_bar.fee, Fee::Base(Qty::new(dec!(0.01))));

    let exit = &r.fills[1];
    assert_eq!(exit.side, Side::Sell);
    assert_eq!(exit.ts, Timestamp::from_millis(3 * HOUR), "t3's open");
    assert_eq!(exit.price, Price::new(dec!(103.89)));
    assert_eq!(exit.qty, Qty::new(dec!(9.99)));
    assert_eq!(exit.fee, Fee::Quote(Notional::new(dec!(1.0378611))));

    let gapped = &r.fills[2];
    assert_eq!(gapped.side, Side::Buy);
    assert_eq!(gapped.ts, Timestamp::from_millis(5 * HOUR), "t5's open");
    assert_eq!(gapped.price, Price::new(dec!(102.11)));
    assert_eq!(gapped.qty, Qty::new(dec!(10)));

    let loss = &r.fills[3];
    assert_eq!(loss.side, Side::Sell);
    assert_eq!(loss.ts, Timestamp::from_millis(6 * HOUR), "t6's open");
    assert_eq!(loss.price, Price::new(dec!(100.89)));
    assert_eq!(loss.qty, Qty::new(dec!(9.99)));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(11.5064478)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 4);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.win_rate, Some(dec!(0.5)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(2011.5064478)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(11.5064478)));
    assert_eq!(r.min_cash, Notional::new(dec!(988.9)));
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.025226651705));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(6.274065));
}
