//! Warm-up parity: a backtest started part-way through the data, with the
//! bars before its start replayed as warm-up, makes the same decisions as
//! one started earlier, the way a live run warms up before trading.
//!
//! Fixture `sma_trend`: BTC 1h, closes 100, 100, 100, 110, 112, 105, 104;
//! sma_period 3. From t0 the strategy first buys at t3 (110 above the
//! average of 100, 100, 110), filled at t4's open. Started at t3 with 3
//! warm-up bars, its first decision at t3 sees the same three closes and
//! buys the same way. Started at t3 cold, it needs three closes of its own
//! and first decides at t5.

use backtest::{BacktestProvider, DataDir, PaperConfig, run_backtest};
use bot_core::{Fill, InstrumentId, Interval, Notional, Timestamp};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

fn dir() -> DataDir {
    DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sma_trend"
    ))
}

fn provider(from_bar: i64) -> BacktestProvider {
    BacktestProvider::from_cache(
        &dir(),
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(from_bar * HOUR),
        Timestamp::from_millis(7 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1009.08)),
        },
    )
    .unwrap()
}

async fn fills(p: &BacktestProvider) -> Vec<Fill> {
    let params = Params::from_toml(
        "coin = \"binance:BTCUSDT\"\nsma_period = 3\nquote_amount = \"1009.08\"\n\
         fee = \"0.001\"\nslippage_bps = \"10\"",
    )
    .unwrap();
    let mut s = strategy::build("sma_trend", &params).unwrap();
    let config = EngineConfig {
        whitelist: vec![btc()],
        history_len: 10,
    };
    engine::run(p, s.as_mut(), params, &config)
        .await
        .unwrap()
        .fills
}

#[tokio::test]
async fn a_warmed_up_start_decides_like_an_earlier_one() {
    let full = fills(&provider(0)).await;
    assert!(!full.is_empty() && full[0].ts.as_millis() == 4 * HOUR);

    let warm = provider(3)
        .warm_up_from_cache(&dir(), Interval::H1, Timestamp::from_millis(3 * HOUR), 3)
        .unwrap();
    assert_eq!(warm.warmup_bars(), vec![(btc(), 3)]);
    assert_eq!(fills(&warm).await, full);

    let cold = fills(&provider(3)).await;
    assert_ne!(cold, full, "without warm-up the first buy is missed");
}

/// Only bars before the start are warm-up, at most the number asked for;
/// fewer if the cache doesn't reach back that far. The report starts at the
/// start: warm-up bars are no part of its equity curve.
#[tokio::test]
async fn warmup_takes_the_last_bars_before_the_start() {
    let from = Timestamp::from_millis(3 * HOUR);
    let two = provider(3)
        .warm_up_from_cache(&dir(), Interval::H1, from, 2)
        .unwrap();
    assert_eq!(two.warmup_bars(), vec![(btc(), 2)]);
    let all = provider(3)
        .warm_up_from_cache(&dir(), Interval::H1, from, 50)
        .unwrap();
    assert_eq!(
        all.warmup_bars(),
        vec![(btc(), 3)],
        "the cache starts at t0"
    );

    let params = Params::from_toml(
        "coin = \"binance:BTCUSDT\"\nquote_amount = \"1009.08\"\n\
         fee = \"0.001\"\nslippage_bps = \"10\"",
    )
    .unwrap();
    let mut s = strategy::build("buy_and_hold", &params).unwrap();
    let config = EngineConfig {
        whitelist: vec![btc()],
        history_len: 10,
    };
    let r = run_backtest(&all, s.as_mut(), params, &config, Interval::H1)
        .await
        .unwrap();
    // buy_and_hold decides on its first candle (t3, not a warm-up bar) and
    // fills at t4's open.
    assert_eq!(r.fills, 1);
    assert_eq!(provider(3).candles().len(), 4);
}
