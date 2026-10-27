//! In-memory provider for engine tests: a fixed event list and a
//! `PaperBroker`. No network, no files.

use async_trait::async_trait;
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Fill, Instrument, InstrumentId, InstrumentSpec,
    Interval, MarketEvent, Notional, Order, OrderId, Position, Price, Qty, Timestamp,
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
        price_band: None,
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
    /// When set, every submit fails with this Binance error code instead of
    /// reaching the paper broker.
    pub submit_error: Option<i64>,
    /// When set, the broker reports this position at start, as a live
    /// broker does with holdings from an earlier run.
    pub prior: Option<Position>,
    /// Fills the broker reports as recovered at startup.
    pub recovered: Vec<Fill>,
    /// When set, a cancelled order stays "cancelling" through the next
    /// settle, as a live venue confirms a cancel only on a later poll.
    pub slow_cancel: bool,
    /// The one instrument served; [`btcusdt`] unless a test swaps in, say,
    /// one with a price band.
    pub instrument: Instrument,
}

impl TestProvider {
    pub fn new(events: Vec<MarketEvent>, slippage_bps: Decimal, fee_rate: Decimal) -> Self {
        Self {
            events,
            classes: vec![AssetClass::Spot],
            slippage_bps,
            fee_rate,
            submit_error: None,
            prior: None,
            recovered: Vec::new(),
            slow_cancel: false,
            instrument: btcusdt(),
        }
    }
}

/// Paper broker whose cancels stay pending through one settle.
struct SlowCancel {
    paper: PaperBroker,
    cancelling: Vec<Order>,
}

#[async_trait]
impl Broker for SlowCancel {
    async fn submit(&mut self, o: Order) -> Result<OrderId, ExchangeError> {
        self.paper.submit(o).await
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        let order = self.paper.open_orders().into_iter().find(|o| o.id == id);
        self.paper.cancel(id).await?;
        self.cancelling.extend(order);
        Ok(())
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.paper.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        self.paper.position(i)
    }
    fn open_orders(&self) -> Vec<Order> {
        self.paper.open_orders()
    }
    fn cancelling_orders(&self) -> Vec<Order> {
        self.cancelling.clone()
    }
}

/// Paper broker whose submits all fail with one venue error code.
struct FailingSubmit(PaperBroker, i64);

/// Paper broker that hands out recovered fills once.
struct Recovering(PaperBroker, Vec<Fill>);

#[async_trait]
impl Broker for Recovering {
    async fn submit(&mut self, o: Order) -> Result<OrderId, ExchangeError> {
        self.0.submit(o).await
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        self.0.cancel(id).await
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.0.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        self.0.position(i)
    }
    fn open_orders(&self) -> Vec<Order> {
        self.0.open_orders()
    }
    fn take_recovered_fills(&mut self) -> Vec<Fill> {
        std::mem::take(&mut self.1)
    }
}

/// Paper broker that reports a fixed position for its instrument.
struct Holding(PaperBroker, Position);

#[async_trait]
impl Broker for Holding {
    async fn submit(&mut self, o: Order) -> Result<OrderId, ExchangeError> {
        self.0.submit(o).await
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        self.0.cancel(id).await
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.0.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        if self.1.instrument() == i {
            self.1.clone()
        } else {
            self.0.position(i)
        }
    }
    fn open_orders(&self) -> Vec<Order> {
        self.0.open_orders()
    }
}

#[async_trait]
impl Broker for FailingSubmit {
    async fn submit(&mut self, _: Order) -> Result<OrderId, ExchangeError> {
        Err(ExchangeError::Venue {
            code: self.1,
            msg: "test".into(),
        })
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        self.0.cancel(id).await
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.0.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        self.0.position(i)
    }
    fn open_orders(&self) -> Vec<Order> {
        self.0.open_orders()
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
        let paper = PaperBroker::new(
            vec![self.instrument.clone()],
            Box::new(NextOpenFillModel),
            Box::new(BpsSlippage::new(self.slippage_bps).unwrap()),
            Box::new(RateFee::new(self.fee_rate, self.fee_rate, FeeAsset::Received).unwrap()),
        );
        Ok(match (self.submit_error, &self.prior) {
            (Some(code), _) => Box::new(FailingSubmit(paper, code)),
            (None, Some(p)) => Box::new(Holding(paper, p.clone())),
            (None, None) if self.slow_cancel => Box::new(SlowCancel {
                paper,
                cancelling: Vec::new(),
            }),
            (None, None) if !self.recovered.is_empty() => {
                Box::new(Recovering(paper, self.recovered.clone()))
            }
            (None, None) => Box::new(paper),
        })
    }

    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = q;
        ids.iter()
            .map(|id| {
                if *id == self.instrument.id {
                    Ok(self.instrument.clone())
                } else {
                    Err(ExchangeError::UnknownInstrument(id.clone()))
                }
            })
            .collect()
    }
}
