use crate::ExchangeError;
use async_trait::async_trait;
use bot_core::{AssetClass, Fill, Instrument, InstrumentId, MarketEvent, Order, OrderId, Position};
use storage::{HoldingRepository, Ledger, OrderJournal};

/// The persistence a venue broker keeps its books in: holdings to start
/// from, an idempotent fill ledger, and the order journal. Any type with
/// all three (`storage::SqlStore`) qualifies.
pub trait Books: Ledger + OrderJournal + HoldingRepository {}
impl<T: Ledger + OrderJournal + HoldingRepository + ?Sized> Books for T {}

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

    /// Orders cancelled but not yet confirmed closed with every fill
    /// reported: they may still fill. Kept apart from `open_orders` so a
    /// strategy won't cancel twice, and copied into `Context` so it can
    /// wait until a cancelled order's last fill is in. Default: none (a
    /// broker whose cancel is final at once, like the paper broker).
    fn cancelling_orders(&self) -> Vec<Order> {
        Vec::new()
    }

    /// Fills that happened while the bot was down, recovered by the
    /// broker's startup reconcile, each returned once. Already in the
    /// positions; the engine hands them to the strategy's `on_fill` so it
    /// learns what became of its orders. Default: none (a paper broker
    /// never misses a fill).
    fn take_recovered_fills(&mut self) -> Vec<Fill> {
        Vec::new()
    }

    /// The id the venue knows order `id` by, for log lines only: never a
    /// decision input, so backtest and live stay identical. A live broker
    /// returns the client order id it sent; a fill recovered from an
    /// earlier run already carries that id. Default: `id` itself (the
    /// paper broker has no venue).
    fn client_order_id(&self, id: &OrderId) -> String {
        id.to_string()
    }
}
