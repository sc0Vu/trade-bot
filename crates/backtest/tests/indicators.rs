//! stochastic, supertrend and ichimoku over one 30-candle BTC fixture,
//! through the cache, BacktestProvider and the engine.
//!
//! Fixture (`tests/fixtures/indicators`): BTC 1h, each bar opening at the
//! previous close, closes 100, 102, 104, 103, 106, 108, 107, 110, 112,
//! 111, 109, 106, 103, 100, 98, 95, 96, 94, 97, 99, 98, 101, 104, 103,
//! 106, 109, 108, 111, 113, 112 (a rally, a slide, a recovery), wicks of
//! 1, 0.5, 1.5, 0.8 beyond open/close in turn. Cash 2000, quote 1000 per
//! buy, fee 0.1% in the received asset, slippage 10 bps rounded to the
//! 0.01 tick against the trader.
//!
//! Expected fills come from a separate Python simulation of each rule,
//! the fill model and risk, not from this code.

use backtest::{BacktestProvider, DataDir, PaperConfig};
use bot_core::{InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

async fn run(strategy: &str, params: &str) -> RunReport {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/indicators"
    ));
    let provider = BacktestProvider::from_cache(
        &dir,
        &[btc()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(30 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(2000)),
        },
    )
    .unwrap();
    let params = Params::from_toml(&format!(
        "coin = \"binance:BTCUSDT\"\nquote_amount = \"1000\"\nfee = \"0.001\"\n\
         slippage_bps = \"10\"\n{params}"
    ))
    .unwrap();
    let mut s = strategy::build(strategy, &params).unwrap();
    let config = EngineConfig {
        whitelist: vec![btc()],
        history_len: 20,
        ..Default::default()
    };
    let r = engine::run(&provider, s.as_mut(), params, &config)
        .await
        .unwrap();
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    r
}

fn fills(r: &RunReport) -> Vec<(i64, Side, Price, Qty)> {
    r.fills
        .iter()
        .map(|f| (f.ts.as_millis() / HOUR, f.side, f.price, f.qty))
        .collect()
}

/// k 3, d 2, oversold 30, overbought 70: one cross up in the slide's
/// trough (decided at t14, filled t15), one cross down after the bounce.
#[tokio::test]
async fn stochastic_buys_the_trough_and_sells_the_bounce() {
    let r = run(
        "stochastic",
        "k_period = 3\nd_period = 2\noversold = \"30\"\noverbought = \"70\"",
    )
    .await;
    assert_eq!(
        fills(&r),
        vec![
            (
                15,
                Side::Buy,
                Price::new(dec!(98.10)),
                Qty::new(dec!(10.19367))
            ),
            (
                21,
                Side::Sell,
                Price::new(dec!(97.90)),
                Qty::new(dec!(10.18347))
            ),
        ]
    );
    assert_eq!(
        r.positions[0].realized_pnl().get().round_dp(12),
        dec!(-4.033654118405)
    );
}

/// period 3, mult 1.5, window 8: long in the rally, out on the slide,
/// back in on the recovery.
#[tokio::test]
async fn supertrend_rides_the_rally_and_the_recovery() {
    let r = run("supertrend", "period = 3\nmult = \"1.5\"\nwindow = 8").await;
    assert_eq!(
        fills(&r),
        vec![
            (
                8,
                Side::Buy,
                Price::new(dec!(110.11)),
                Qty::new(dec!(9.08182))
            ),
            (
                12,
                Side::Sell,
                Price::new(dec!(105.89)),
                Qty::new(dec!(9.07273))
            ),
            (
                19,
                Side::Buy,
                Price::new(dec!(97.10)),
                Qty::new(dec!(10.29866))
            ),
        ]
    );
    assert_eq!(r.positions[0].qty(), Qty::new(dec!(10.28836952)));
    assert_eq!(
        r.positions[0].realized_pnl().get().round_dp(12),
        dec!(-40.247630278299)
    );
}

/// tenkan 2, kijun 3, senkou_b 4, displacement 2: above the cloud early in
/// the rally, out when the slide breaks below it, back in on the recovery.
#[tokio::test]
async fn ichimoku_follows_the_cloud() {
    let r = run(
        "ichimoku",
        "tenkan = 2\nkijun = 3\nsenkou_b = 4\ndisplacement = 2",
    )
    .await;
    assert_eq!(
        fills(&r),
        vec![
            (
                7,
                Side::Buy,
                Price::new(dec!(107.11)),
                Qty::new(dec!(9.33619))
            ),
            (
                11,
                Side::Sell,
                Price::new(dec!(108.89)),
                Qty::new(dec!(9.32685))
            ),
            (
                21,
                Side::Buy,
                Price::new(dec!(98.10)),
                Qty::new(dec!(10.19367))
            ),
        ]
    );
    assert_eq!(
        r.positions[0].realized_pnl().get().round_dp(12),
        dec!(14.586193401098)
    );
}
