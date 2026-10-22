use bot_core::{InstrumentId, OrderId};
use thiserror::Error;

/// Failure at the venue boundary. Messages carry ids and rules, never money
/// values, so they are safe to log.
#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("instrument {0} is not served by this broker")]
    UnknownInstrument(InstrumentId),
    #[error("no open order {0}")]
    UnknownOrder(OrderId),
    #[error("order id {0} already in use")]
    DuplicateOrder(OrderId),
    /// The venue (or the paper broker standing in for it) refused the order.
    #[error("order rejected: {0}")]
    Rejected(&'static str),
    /// The market data feed failed; the engine stops rather than trade on a
    /// gap.
    #[error("market data feed failed: {0}")]
    Feed(String),
}
