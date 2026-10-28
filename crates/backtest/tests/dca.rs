//! dca over the 7-candle BTC fixture shared with sma_trend and buy_and_hold
//! (`tests/fixtures/sma_trend`: flat candles at 100, 100, 100, 110, 112,
//! 105, 104): 300 USDT every hour, at most 3 buys, from 1000 cash. Fee 0.1%
//! in the received asset; slippage 10 bps on taker fills, rounded to the
//! 0.01 tick against the trader.

use backtest::{BacktestProvider, DataDir, PaperConfig, run_backtest};
use bot_core::{Fee, InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::EngineConfig;
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
            initial_cash: Notional::new(dec!(1000)),
        },
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           quote_amount = "300"
           every = "hour"
           max_buys = 3
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

/// Worked by hand (Python decimal). Buys decided at t0, t1, t2 fill at the
/// next opens 100, 100, 110, slipped up to 100.10, 100.10, 110.11:
///   300 / 100.10 = 2.997002... -> 2.99700 BTC, spend 299.9997, fee
///   0.002997 BTC (twice); 300 / 110.11 = 2.724547... -> 2.72454 BTC,
///   spend 299.9990994, fee 0.00272454 BTC.
/// Hold 8.70982146 BTC at cost 899.9984994; cash 100.0015006. No buy at t3
/// (max_buys). Final equity at close 104: 100.0015006 + 8.70982146 x 104
/// = 1005.82293244. Buy and hold on the same data ends at 1047.35 from
/// 1009.08: averaging into a rise pays more per unit.
#[tokio::test]
async fn buys_on_schedule_until_max_buys() {
    let p = params();
    let mut s = strategy::build("dca", &p).unwrap();
    let r = engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap();
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<_> = r
        .fills
        .iter()
        .map(|f| {
            (
                f.side,
                f.ts.as_millis() / HOUR,
                f.price,
                f.qty,
                f.fee.clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                Side::Buy,
                1,
                Price::new(dec!(100.10)),
                Qty::new(dec!(2.997)),
                Fee::Base(Qty::new(dec!(0.002997)))
            ),
            (
                Side::Buy,
                2,
                Price::new(dec!(100.10)),
                Qty::new(dec!(2.997)),
                Fee::Base(Qty::new(dec!(0.002997)))
            ),
            (
                Side::Buy,
                3,
                Price::new(dec!(110.11)),
                Qty::new(dec!(2.72454)),
                Fee::Base(Qty::new(dec!(0.00272454)))
            ),
        ]
    );
    let pos = &r.positions[0];
    assert_eq!(pos.qty(), Qty::new(dec!(8.70982146)));
    assert_eq!(pos.cost_basis(), Notional::new(dec!(899.9984994)));

    let p = params();
    let mut s = strategy::build("dca", &p).unwrap();
    let report = run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap();
    assert_eq!(report.fills, 3);
    assert_eq!(report.min_cash, Notional::new(dec!(100.0015006)));
    assert_eq!(report.final_equity, Notional::new(dec!(1005.82293244)));
}
