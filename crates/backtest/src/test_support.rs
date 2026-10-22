use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Instrument, InstrumentId, InstrumentSpec, Interval,
    Notional, Price, Qty, Timestamp,
};
use rust_decimal_macros::dec;
use std::path::PathBuf;

pub const HOUR: i64 = 3_600_000;

pub fn btcusdt() -> Instrument {
    Instrument::new(InstrumentSpec {
        id: "binance:BTCUSDT".parse().unwrap(),
        class: AssetClass::Spot,
        base: Asset::new("BTC").unwrap(),
        quote: Asset::new("USDT").unwrap(),
        tick_size: Price::new(dec!(0.01)),
        lot_size: Qty::new(dec!(0.00001)),
        min_qty: Qty::new(dec!(0.00001)),
        min_notional: Notional::new(dec!(5)),
    })
    .unwrap()
}

/// The `i`th 1h candle of `id`, prices `[open, high, low, close]`.
pub fn candle(id: &InstrumentId, i: i64, ohlc: [&str; 4]) -> Candle {
    let [o, h, l, c] = ohlc.map(|s| s.parse::<Price>().unwrap());
    Candle::new(CandleSpec {
        instrument: id.clone(),
        interval: Interval::H1,
        open_time: Timestamp::from_millis(i * HOUR),
        close_time: Timestamp::from_millis(i * HOUR + HOUR - 1),
        open: o,
        high: h,
        low: l,
        close: c,
        volume: Qty::new(dec!(1)),
        quote_volume: Notional::new(c.get()),
    })
    .unwrap()
}

/// Fresh empty directory under the system temp dir, unique per test name.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trade-bot-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
