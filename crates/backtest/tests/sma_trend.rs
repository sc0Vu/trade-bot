//! sma_trend over a fixed 7-candle BTC fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/sma_trend`): BTC 1h, flat candles (open = high
//! = low = close) at 100, 100, 100, 110, 112, 105, 104. sma_period 3, no
//! band. Fee 0.1% maker and taker in the received asset; slippage 10 bps
//! on taker fills, rounded to the 0.01 tick against the trader.

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
           sma_period = 3
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
    let mut s = strategy::build("sma_trend", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("sma_trend", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner, 2026-10-22.
///   t2: SMA(100, 100, 100) = 100, close 100 not above: nothing.
///   t3: SMA = 310/3 = 103.33..., close 110 above -> buy, quote 1009.08.
///   t4 open 112 * 1.001 = 112.112 -> 112.12 (tick up): 1009.08 / 112.12 =
///      9 BTC exactly, fee 0.009 BTC, hold 8.991 at cost 1009.08, cash 0.
///   t4: SMA(100, 110, 112) = 107.33..., close 112 above: hold.
///   t5: SMA(110, 112, 105) = 109, close 105 below -> sell 8.991.
///   t6 open 104 * 0.999 = 103.896 -> 103.89 (tick down): 8.991 * 103.89 =
///      934.07499, fee 0.93407499 USDT, proceeds 933.14091501; realized
///      933.14091501 - 1009.08 = -75.93908499; flat, no dust.
///   t6: SMA(112, 105, 104) = 107, flat and close below: nothing.
/// Equity at closes: 1009.08 x4, 1006.992, 944.055, 933.14091501. Max
/// drawdown 75.93908499 / 1009.08 = 0.075255762665 (12 dp).
/// Sharpe -47.862146 (per-bar returns, sample sd, x sqrt(8760)).
#[tokio::test]
async fn buys_the_cross_up_and_sells_the_cross_down() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 2);

    let (buy, sell) = (&r.fills[0], &r.fills[1]);
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(buy.ts, Timestamp::from_millis(4 * HOUR), "t4's open");
    assert_eq!(buy.price, Price::new(dec!(112.12)));
    assert_eq!(buy.qty, Qty::new(dec!(9)));
    assert_eq!(buy.fee, Fee::Base(Qty::new(dec!(0.009))));

    assert_eq!(sell.side, Side::Sell);
    assert_eq!(sell.ts, Timestamp::from_millis(6 * HOUR), "t6's open");
    assert_eq!(sell.price, Price::new(dec!(103.89)));
    assert_eq!(sell.qty, Qty::new(dec!(8.991)));
    assert_eq!(sell.fee, Fee::Quote(Notional::new(dec!(0.93407499))));

    let pos = &r.positions[0];
    assert!(pos.is_flat());
    assert_eq!(pos.realized_pnl(), Notional::new(dec!(-75.93908499)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 2);
    assert_eq!(r.closed_trades, 1);
    assert_eq!(r.win_rate, Some(dec!(0)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(933.14091501)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(-75.93908499)));
    // The buy spends the whole budget and never more.
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.075255762665));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-47.862146));
}
