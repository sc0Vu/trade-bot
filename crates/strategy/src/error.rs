use thiserror::Error;

/// Failure to find or build a strategy.
#[derive(Debug, Error)]
pub enum StrategyError {
    #[error("no strategy registered as {0:?}")]
    Unknown(String),
    /// Two registrations share a name; which one runs would depend on link
    /// order, so neither does.
    #[error("strategy name {0:?} registered more than once")]
    Duplicate(&'static str),
    /// `strategy.params` doesn't deserialize into the strategy's params type.
    #[error("invalid params for {strategy:?}: {reason}")]
    Params { strategy: String, reason: String },
}
