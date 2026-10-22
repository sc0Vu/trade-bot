use bot_core::{InstrumentId, Timestamp};
use exchange::ExchangeError;
use thiserror::Error;

/// Why a run stopped. Rejected signals are not errors: risk refusals and
/// venue refusals of one order ([`ExchangeError::is_order_rejection`]) are
/// reported in `RunReport::rejections` and the run continues. Any other
/// broker error lands here.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    /// The feed went backwards in time. Continuing could let a strategy act
    /// on data out of order, so the run stops.
    #[error("event at {got} arrived after event at {last}")]
    OutOfOrder { last: Timestamp, got: Timestamp },
    /// The provider can't trade this instrument's asset class.
    #[error("provider does not serve the asset class of {0}")]
    UnsupportedAssetClass(InstrumentId),
}
