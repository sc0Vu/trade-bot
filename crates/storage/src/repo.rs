use crate::StorageError;
use async_trait::async_trait;
use bot_core::{Fill, Instrument, InstrumentId, Position, Timestamp};
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
}
