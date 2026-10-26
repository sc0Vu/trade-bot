//! avwap through the engine, the paper broker and the report, on five bars
//! worked by hand.
//!
//! One UTC day of 1h bars, each flat at its price and trading 1 BTC for
//! that price in USDT, so the day-anchored VWAP is the mean of the prices
//! so far. 1000 USDT per entry and 1000 cash. Fee 0.1% in the received
//! asset; slippage 10 bps on market fills, to the tick against the trader.
//! BTCUSDT filters: tick 0.01, lot 0.00001.

use backtest::{BacktestProvider, DataDir, PaperConfig, run_backtest};
use bot_core::{Candle, CandleSpec, InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

fn bar(i: i64, price: &str) -> Candle {
    let p: Price = price.parse().unwrap();
    Candle::new(CandleSpec {
        instrument: btc(),
        interval: Interval::H1,
        open_time: Timestamp::from_millis(i * HOUR),
        close_time: Timestamp::from_millis(i * HOUR + HOUR - 1),
        open: p,
        high: p,
        low: p,
        close: p,
        volume: Qty::new(dec!(1)),
        quote_volume: Notional::new(p.get()),
    })
    .unwrap()
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ratio_hop"
    ));
    BacktestProvider::new(
        vec![dir.read_instrument(&btc()).unwrap()],
        ["100", "102", "104", "99", "98"]
            .iter()
            .enumerate()
            .map(|(i, p)| bar(i as i64, p))
            .collect(),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1000)),
        },
    )
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           anchor = "day"
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc()],
        history_len: 30,
    }
}

/// Worked by hand (Python decimal):
///   b0 close 100 = AVWAP 100: nothing. b1 close 102 > AVWAP 101: buy.
///   b2 fills at the open slipped, 104 * 1.001 = 104.104 -> 104.11:
///      lot(1000 / 104.11) = 9.60522 BTC, cost 999.9994542, net of the
///      base fee 9.59561478. Close 104 > AVWAP 102: hold.
///   b3 close 99 < AVWAP 101.25: sell lot 9.59561.
///   b4 fills at 98 * 0.999 = 97.902 -> 97.90: 939.410219 less fee
///      0.939410219 = 938.470808781. 0.00000478 BTC dust is left (cash to
///      the strategy: too small to sell).
/// Cash 1000 - 999.9994542 + 938.470808781 = 938.471354581 (lowest
/// 0.0005458, after the buy); equity with the dust at 98 = 938.471823021.
#[tokio::test]
async fn buys_above_the_anchored_vwap_and_sells_below() {
    let p = params();
    let mut s = strategy::build("avwap", &p).unwrap();
    let r = engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap();
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<_> = r.fills.iter().map(|f| (f.side, f.qty, f.price)).collect();
    assert_eq!(
        got,
        vec![
            (Side::Buy, Qty::new(dec!(9.60522)), Price::new(dec!(104.11))),
            (Side::Sell, Qty::new(dec!(9.59561)), Price::new(dec!(97.90))),
        ]
    );

    let p = params();
    let mut s = strategy::build("avwap", &p).unwrap();
    let report = run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap();
    assert_eq!(report.final_equity, Notional::new(dec!(938.471823021)));
    assert_eq!(report.min_cash, Notional::new(dec!(0.0005458)));
    assert_eq!(report.closed_trades, 1);
}
