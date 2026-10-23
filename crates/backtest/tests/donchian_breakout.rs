//! donchian_breakout over a fixed 8-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/donchian_breakout`): BTC 1h, flat candles at
//! 100, 100, 100, 105, 108, 104, 99, 98 — a breakout that fails.
//! channel_period 3. Fee 0.1% maker and taker in the received asset;
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
        "/tests/fixtures/donchian_breakout"
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
            initial_cash: Notional::new(dec!(972.99)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           channel_period = 3
           quote_amount = "972.99"
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
    let mut s = strategy::build("donchian_breakout", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("donchian_breakout", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner,
/// 2026-10-23. Channel = high / low of the
/// 3 candles before the current one.
///   t3: 105 > channel high 100, flat -> buy 972.99.
///   t4 open 108 * 1.001 = 108.108 -> 108.11 (tick up): 972.99 / 108.11 = 9
///      BTC exactly, fee 0.009 BTC, hold 8.991 at cost 972.99, cash 0.
///   t4: channel (100, 100, 105), long: hold. t5: 104 inside (100..108).
///   t6: 99 < channel low of (105, 108, 104) = 104 -> sell 8.991.
///   t7 open 98 * 0.999 = 97.902 -> 97.90 (tick down): 8.991 * 97.90 =
///      880.2189, fee 0.8802189 USDT, proceeds 879.3386811, realized
///      -93.6513189; flat.
///   t7: 98 under channel high 108, flat: nothing.
/// Equity at closes: 972.99 x4, 971.028, 935.064, 890.109, 879.3386811.
/// Max drawdown 93.6513189 / 972.99 = 0.096251060031 (12 dp). Sharpe
/// -65.969762.
#[tokio::test]
async fn buys_the_breakout_and_sells_the_breakdown() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);

    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(4 * HOUR), "t4's open");
    assert_eq!(buy.price, Price::new(dec!(108.11)));
    assert_eq!(buy.qty, Qty::new(dec!(9)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.009))));

    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(7 * HOUR), "t7's open");
    assert_eq!(sell.price, Price::new(dec!(97.90)));
    assert_eq!(sell.qty, Qty::new(dec!(8.991)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.8802189))));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(-93.6513189)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(0)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(879.3386811)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(-93.6513189)));
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.096251060031));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-65.969762));
}
