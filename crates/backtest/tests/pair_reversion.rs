//! pair_reversion over a fixed 12-candle BTC + ETH fixture, through the
//! cache, BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/pair_reversion`), 1h flat candles:
//! BTC 100, 100, 100, 100, 104, 100, 96, 100, 104, 104, 100, 97;
//! ETH 10, 10, 10, 10, 10, 10, 10, 10.2, 10, 10.3, 10.1, 10.
//! The BTC/ETH ratio swings around 10. period 4, entry_z 1, band 0.05,
//! capital 10000 = the broker's cash. Fee 0.1% in the received asset;
//! slippage 10 bps on taker fills, rounded to tick (BTC 0.01, ETH 0.001)
//! against the trader.

use backtest::{BacktestProvider, DataDir, PaperConfig, Report, run_backtest};
use bot_core::{InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

fn eth() -> InstrumentId {
    "binance:ETHUSDT".parse().unwrap()
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pair_reversion"
    ));
    BacktestProvider::from_cache(
        &dir,
        &[btc(), eth()],
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(12 * HOUR),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           period = 4
           entry_z = "1"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc(), eth()],
        history_len: 10,
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("pair_reversion", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("pair_reversion", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// From a separate Python simulation of the fill model, risk and the rule.
///   t3: ratios 10 ×4, no spread → nothing (held nothing yet).
///   t4: ratio 10.4 above the mean by > 1 sd → ETH cheap → buy 10000 ETH;
///       fills t5 open 10 → 10.010: 999.00099.
///   t6: ratio 9.6 → BTC cheap → sell all ETH, buy BTC with the estimated
///       proceeds; both fill at t7's open (BTC's candle first: the buy
///       fills before the sale funding it, as the broker allows).
///   t8: ratio 10.4 → ETH cheap again → switch back (fills t9).
///   t9–t11: inside the band → keep ETH.
#[tokio::test]
async fn switches_into_whichever_coin_is_cheap() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<(i64, String, Side, Price, Qty)> = r
        .fills
        .iter()
        .map(|f| {
            (
                f.ts.as_millis() / HOUR,
                f.instrument.symbol().to_string(),
                f.side,
                f.price,
                f.qty,
            )
        })
        .collect();
    let row = |t, sym: &str, side, p, q| (t, sym.to_owned(), side, Price::new(p), Qty::new(q));
    assert_eq!(
        got,
        vec![
            row(5, "ETHUSDT", Side::Buy, dec!(10.010), dec!(999.00099)),
            row(7, "BTCUSDT", Side::Buy, dec!(100.10), dec!(99.49112)),
            row(7, "ETHUSDT", Side::Sell, dec!(10.189), dec!(998.00198)),
            row(9, "BTCUSDT", Side::Sell, dec!(103.89), dec!(99.39162)),
            row(9, "ETHUSDT", Side::Buy, dec!(10.311), dec!(1019.73364)),
        ]
    );
    let (b, e) = (&r.positions[0], &r.positions[1]);
    assert_eq!(b.qty(), Qty::new(dec!(0.00000888)));
    assert_eq!(e.qty(), Qty::new(dec!(1018.71391537)));
    assert_eq!(b.realized_pnl().get().round_dp(12), dec!(356.409384175978));
    assert_eq!(e.realized_pnl().get().round_dp(12), dec!(158.473712426160));
}

/// Cash 0.40855450398 + 0.00000888·97 + 1018.71391537·10. The t7 buy
/// fills before the sale funding it, yet min_cash never dips below zero:
/// fills of one instant count together.
#[tokio::test]
async fn report_matches_the_simulated_run() {
    let r = report().await;
    assert_eq!(r.fills, 5);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.win_rate, Some(dec!(1)));
    assert_eq!(r.final_equity, Notional::new(dec!(10187.54856956398)));
    assert!(r.min_cash >= Notional::ZERO);
}
