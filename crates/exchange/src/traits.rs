use crate::ExchangeError;
use async_trait::async_trait;
use bot_core::{AssetClass, Fill, Instrument, InstrumentId, MarketEvent, Order, OrderId, Position};

/// Which instruments to fetch reference data for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstrumentQuery {
    /// Exactly these (the whitelist). Unknown ids are an error, not skipped,
    /// so a typo can't silently shrink the universe.
    Ids(Vec<InstrumentId>),
}

/// A venue and what it can do. Mints the market data feed and the broker,
/// and declares the asset classes it serves, so the engine can refuse a
/// strategy pointed at a venue that can't trade its instruments before
/// anything runs.
///
/// Backtest and live are both providers; the engine never knows which.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    fn asset_classes(&self) -> &[AssetClass];

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError>;

    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError>;

    /// Reference data (tick, lot, minimums) for the requested instruments.
    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError>;
}

/// Ordered stream of market events. Backtest replays the same event types
/// live produces, so parity holds for every variant.
#[async_trait]
pub trait MarketData: Send {
    /// The next event, `Ok(None)` at end of stream. Events arrive in
    /// non-decreasing `ts()` order.
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError>;
}

/// Order execution. Submits and cancels orders, reports fills, and owns the
/// positions that result. Never values an instrument (invariant 4).
#[async_trait]
pub trait Broker: Send {
    /// Accepts an order to execute on a later event. Returns its id.
    async fn submit(&mut self, order: Order) -> Result<OrderId, ExchangeError>;

    /// Cancels an order that hasn't filled yet.
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError>;

    /// Settles pending orders against `event`, the newest market event, and
    /// returns the resulting fills in execution order. The engine calls this
    /// before the strategy sees `event`, so an order decided on event t
    /// fills no earlier than t+1. A live broker returns fills the venue
    /// reported since the last call.
    async fn settle(&mut self, event: &MarketEvent) -> Result<Vec<Fill>, ExchangeError>;

    /// Current position; flat if never traded.
    fn position(&self, instrument: &InstrumentId) -> Position;

    /// Orders accepted but not yet filled, cancelled or expired, in
    /// submission order. The engine copies this into the strategy's
    /// `Context` so a strategy can cancel by id without a broker handle.
    fn open_orders(&self) -> Vec<Order>;
}
