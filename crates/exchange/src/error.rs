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
    /// HTTP or transport failure talking to a venue.
    #[error("venue request failed: {0}")]
    Http(String),
    /// The venue refused the request with its own error code (Binance
    /// `{"code":-2010,"msg":"..."}`). `msg` is the venue's text.
    #[error("venue error {code}: {msg}")]
    Venue { code: i64, msg: String },
    /// Credentials missing or unusable. Names the env var, never a value.
    #[error("credentials: {0}")]
    Credentials(String),
    /// The venue answered with something this client can't use. Fails
    /// loudly: a silently misparsed price is worse than no data.
    #[error("unexpected venue response: {0}")]
    Decode(String),
}
