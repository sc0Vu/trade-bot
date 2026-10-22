//! Strategies shipped with the bot. Each file registers itself.

pub mod buy_and_hold;
pub mod donchian_breakout;
pub mod dual_ma_cross;
pub mod momentum_rotation;
pub mod ratio_hop;
pub mod rsi_reversion;
pub mod sma_trend;

#[cfg(test)]
pub(crate) mod test_support {
    use bot_core::{
        Asset, AssetClass, Candle, CandleSpec, Instrument, InstrumentSpec, Interval, Notional,
        Price, Qty, Timestamp,
    };
    use rust_decimal_macros::dec;

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

    /// The `i`th 1h BTCUSDT candle, flat at `price`.
    pub fn candle(i: i64, price: &str) -> Candle {
        let p: Price = price.parse().unwrap();
        let open = i * 3_600_000;
        Candle::new(CandleSpec {
            instrument: btcusdt().id.clone(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(open),
            close_time: Timestamp::from_millis(open + 3_599_999),
            open: p,
            high: p,
            low: p,
            close: p,
            volume: Qty::new(dec!(1)),
            quote_volume: Notional::new(p.get()),
        })
        .unwrap()
    }
}
