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

/// Binance codes that refuse one order and say nothing about the account:
/// `-1013` filter failure, `-2010` new order rejected (e.g. insufficient
/// balance), `-2011` cancel rejected (e.g. order already closed).
const BINANCE_ORDER_REJECTIONS: [i64; 3] = [-1013, -2010, -2011];

impl ExchangeError {
    /// True when the venue refused this one order or cancel and the next
    /// one may well succeed, so the engine records it and keeps running.
    ///
    /// Everything else stops the run. Bad keys, IP bans and missing
    /// permissions (`-2015`), clock skew, rate limits and decode failures
    /// would refuse every later order too, and a run that keeps going
    /// looks healthy while trading nothing. A transport failure on submit
    /// is fatal as well: the order may have reached the venue, and a
    /// guessed state is worse than a stop.
    pub fn is_order_rejection(&self) -> bool {
        match self {
            Self::Rejected(_) | Self::UnknownOrder(_) | Self::UnknownInstrument(_) => true,
            Self::Venue { code, .. } => BINANCE_ORDER_REJECTIONS.contains(code),
            Self::DuplicateOrder(_)
            | Self::Feed(_)
            | Self::Http(_)
            | Self::Credentials(_)
            | Self::Decode(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue(code: i64) -> ExchangeError {
        ExchangeError::Venue {
            code,
            msg: String::new(),
        }
    }

    #[test]
    fn order_level_refusals_are_rejections() {
        assert!(ExchangeError::Rejected("sell exceeds holding").is_order_rejection());
        assert!(venue(-1013).is_order_rejection());
        assert!(venue(-2010).is_order_rejection());
        assert!(venue(-2011).is_order_rejection());
    }

    #[test]
    fn account_and_transport_failures_are_fatal() {
        // -2015 bad key/IP/permission, -2014 bad key format, -1022 bad
        // signature, -1021 clock skew, -1003 rate limit.
        for code in [-2015, -2014, -1022, -1021, -1003] {
            assert!(!venue(code).is_order_rejection(), "{code}");
        }
        assert!(!ExchangeError::Http("timeout".into()).is_order_rejection());
        assert!(!ExchangeError::Credentials("BINANCE_API_KEY".into()).is_order_rejection());
        assert!(!ExchangeError::Decode("bad".into()).is_order_rejection());
    }
}
