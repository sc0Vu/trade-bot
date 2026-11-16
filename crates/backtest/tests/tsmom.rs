//! tsmom over a fixed 12-candle BTC + ETH fixture, through the cache,
//! BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/tsmom`), 1h flat candles:
//! BTC 100, 101, 103, 102, 105, 107, 106, 104, 101, 99, 100, 102 (up, then
//! down, then turning); ETH 10, 9.9, 9.7, 9.8, 9.6, 9.5, 9.7, 9.9, 10.1,
//! 10.3, 10.2, 10.4 (down, then up). lookback 3, vol_period 3, target_vol
//! 1.5, band 0.05, capital 10000 = the broker's cash. Fee 0.1% in the
//! received asset; slippage 10 bps on taker fills, rounded to tick against
//! the trader.

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
    let dir = DataDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tsmom"));
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
           lookback = 3
           target_vol = "1.5"
           vol_period = 3
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
        ..Default::default()
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("tsmom", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("tsmom", &p).unwrap();
    run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap()
}

/// From a separate Python simulation (decimal, 28 digits, exact square
/// roots) of the fill model, risk and the rule, not from this code.
/// Weight = min(1, 1.5 / (sd of 3 hourly returns · √8760)) / 2 if the
/// 3-bar return is positive, else 0.
///   t3: BTC 102 > 100 (vol 1.39 → capped: 0.5); ETH 9.8 < 10 → 0.
///       Buy 5000 BTC, filled at t4's open 105 → 105.11: 47.56921.
///   t4: BTC vol up → 0.3931, held ≈ 0.50 → sell 10.12488 (t5).
///   t5, t6: 0.3954, 0.3994: in band.
///   t7: BTC 104 < 105 → 0, sell all 37.39676 (base-fee dust stays);
///       ETH 9.9 > 9.8 → 0.4441: buy 4427.62524064 (both at t8).
///   t8: ETH 0.5 → top up 502.17103756 (t9). t9, t10: in band.
///   t11: BTC up again → buy decided on the last bar: never fills.
#[tokio::test]
async fn follows_each_coins_own_trend() {
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
            row(4, "BTCUSDT", Side::Buy, dec!(105.11), dec!(47.56921)),
            row(5, "BTCUSDT", Side::Sell, dec!(106.89), dec!(10.12488)),
            row(8, "BTCUSDT", Side::Sell, dec!(100.89), dec!(37.39676)),
            row(8, "ETHUSDT", Side::Buy, dec!(10.111), dec!(437.90181)),
            row(9, "ETHUSDT", Side::Buy, dec!(10.311), dec!(48.70245)),
        ]
    );
    let (b, e) = (&r.positions[0], &r.positions[1]);
    assert_eq!(b.qty(), Qty::new(dec!(0.00000079)));
    assert_eq!(e.qty(), Qty::new(dec!(486.11765574)));
    assert_eq!(b.realized_pnl().get().round_dp(12), dec!(-149.647247919580));
    assert_eq!(e.realized_pnl(), Notional::ZERO);
}

/// Cash 4920.5565061004 + 0.00000079·102 + 486.11765574·10.4.
#[tokio::test]
async fn report_matches_the_simulated_run() {
    let r = report().await;
    assert_eq!(r.fills, 5);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(9976.1802063764)));
    assert!(r.min_cash >= Notional::ZERO);
}
