//! In-memory provider for engine tests: a fixed event list and a
//! `PaperBroker`. No network, no files.

use async_trait::async_trait;
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Instrument, InstrumentSpec, Interval, MarketEvent,
    Notional, Price, Qty, Timestamp,
};
use exchange::paper::{BpsSlippage, FeeAsset, NextOpenFillModel, PaperBroker, RateFee};
use exchange::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub const HOUR: i64 = 3_600_000;

/// Binance BTCUSDT spot filters: tick 0.01, lot 0.00001, min notional 5.
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

/// The `i`th 1h BTCUSDT candle, prices `[open, high, low, close]`.
pub fn candle(i: i64, ohlc: [&str; 4]) -> Candle {
    let [o, h, l, c] = ohlc.map(|s| s.parse::<Price>().unwrap());
    Candle::new(CandleSpec {
        instrument: btcusdt().id.clone(),
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

pub struct TestProvider {
    pub events: Vec<MarketEvent>,
    pub classes: Vec<AssetClass>,
    pub slippage_bps: Decimal,
    /// Taker and maker rate, charged in the received asset (Binance spot).
    pub fee_rate: Decimal,
}

impl TestProvider {
    pub fn new(events: Vec<MarketEvent>, slippage_bps: Decimal, fee_rate: Decimal) -> Self {
        Self {
            events,
            classes: vec![AssetClass::Spot],
            slippage_bps,
            fee_rate,
        }
    }
}

struct VecFeed(std::vec::IntoIter<MarketEvent>);

#[async_trait]
impl MarketData for VecFeed {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        Ok(self.0.next())
    }
}

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &str {
        "test"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &self.classes
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(VecFeed(self.events.clone().into_iter())))
    }

    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        Ok(Box::new(PaperBroker::new(
            vec![btcusdt()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(self.slippage_bps).unwrap()),
            Box::new(RateFee::new(self.fee_rate, self.fee_rate, FeeAsset::Received).unwrap()),
        )))
    }

    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = q;
        ids.iter()
            .map(|id| {
                if *id == btcusdt().id {
                    Ok(btcusdt())
                } else {
                    Err(ExchangeError::UnknownInstrument(id.clone()))
                }
            })
            .collect()
    }
}
