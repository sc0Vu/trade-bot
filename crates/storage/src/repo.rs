use crate::StorageError;
use async_trait::async_trait;
use bot_core::{Fill, Instrument, InstrumentId, Position, Side, Timestamp};
use std::fmt;

/// Database id of a stored trade. Assigned on insert, increasing in insert
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TradeId(pub(crate) i64);

impl TradeId {
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for TradeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Append-only log of fills. Trades are never updated or deleted: the log
/// is the record that holdings and reports are derived from.
#[async_trait]
pub trait TradeRepository: Send + Sync {
    /// Appends one fill.
    async fn insert_trade(&self, fill: &Fill) -> Result<TradeId, StorageError>;

    /// The fill stored under `id`, or `None` if there is none.
    async fn get_trade(&self, id: TradeId) -> Result<Option<Fill>, StorageError>;

    /// Fills for `instrument` with `from <= ts < to`, oldest first (ties in
    /// insert order), so replaying them reproduces the position.
    async fn list_trades(
        &self,
        instrument: &InstrumentId,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<(TradeId, Fill)>, StorageError>;
}

/// Current position per instrument, one row each. A cache of the trade log
/// so startup needn't replay every fill.
#[async_trait]
pub trait HoldingRepository: Send + Sync {
    /// Inserts or replaces the holding for `position.instrument()`. Bypasses
    /// the trade log: use it to rebuild from a replay; record live fills
    /// with [`Ledger::record_fill`].
    async fn upsert_holding(&self, position: &Position) -> Result<(), StorageError>;

    /// The stored holding, or `None` if the instrument was never held.
    async fn get_holding(
        &self,
        instrument: &InstrumentId,
    ) -> Result<Option<Position>, StorageError>;

    /// Every stored holding, flat ones included, ordered by instrument.
    async fn list_holdings(&self) -> Result<Vec<Position>, StorageError>;
}

/// Writes that must change trades and holdings together.
#[async_trait]
pub trait Ledger: Send + Sync {
    /// Records one fill atomically. In a single transaction it appends the
    /// fill to the trade log and folds it into the stored holding (flat if
    /// none) with `Position::apply`. Returns the trade id and the new
    /// position.
    ///
    /// Either both writes land or neither does: a fill the position rejects
    /// (`StorageError::Rejected`) or a database failure leaves storage
    /// unchanged. Storage applies the fill itself rather than accepting a
    /// caller-computed position, so the holding can't drift from the log.
    async fn record_fill(
        &self,
        fill: &Fill,
        instrument: &Instrument,
    ) -> Result<(TradeId, Position), StorageError>;

    /// [`record_fill`](Self::record_fill) for a fill the venue reported
    /// under `venue_trade_id`, idempotently: if that trade is already
    /// recorded for this instrument, nothing is written and `None` comes
    /// back. Lets a restart replay the venue's trade history without
    /// counting a fill twice.
    async fn record_venue_fill(
        &self,
        fill: &Fill,
        instrument: &Instrument,
        venue_trade_id: &str,
    ) -> Result<Option<(TradeId, Position)>, StorageError>;
}

/// Where an order stands in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderStatus {
    /// Written before the submit request went out; the venue may or may
    /// not have it.
    Pending,
    /// The venue acknowledged it and gave its own id.
    Open,
    /// Nothing more can fill, and every fill is recorded.
    Closed,
}

/// One journaled order: enough to find it on the venue again and to check
/// its trades, not a full copy of the order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalOrder {
    /// The id sent as the venue's client order id.
    pub client_order_id: String,
    pub instrument: InstrumentId,
    pub side: Side,
    pub status: OrderStatus,
    /// Set once the venue acknowledged the order.
    pub venue_order_id: Option<i64>,
    pub created_at: Timestamp,
}

/// Journal of orders sent to a venue, so a restart can find every order
/// the bot may still have out there.
///
/// Write order matters: [`insert_pending`](Self::insert_pending) *before*
/// the request is sent, [`mark_open`](Self::mark_open) on the ack,
/// [`mark_closed`](Self::mark_closed) only after the order's last fill is
/// recorded. A crash at any point then leaves a row that is either closed
/// or worth checking.
#[async_trait]
pub trait OrderJournal: Send + Sync {
    /// Journals a new order as [`OrderStatus::Pending`]. A reused
    /// `client_order_id` is an error.
    async fn insert_pending(
        &self,
        client_order_id: &str,
        instrument: &InstrumentId,
        side: Side,
        created_at: Timestamp,
    ) -> Result<(), StorageError>;

    /// Records the venue's ack. Unknown `client_order_id` is an error.
    async fn mark_open(
        &self,
        client_order_id: &str,
        venue_order_id: i64,
    ) -> Result<(), StorageError>;

    /// Marks the order finished. Unknown `client_order_id` is an error.
    async fn mark_closed(&self, client_order_id: &str) -> Result<(), StorageError>;

    /// Every order not yet closed, oldest first.
    async fn unfinished_orders(&self) -> Result<Vec<JournalOrder>, StorageError>;
}
