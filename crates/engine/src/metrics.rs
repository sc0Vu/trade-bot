//! Metric names the engine records, through the `metrics` facade.
//!
//! Nothing is recorded unless the binary installs a recorder (the CLI's
//! Prometheus exporter); backtests and tests install none, so these calls
//! cost nothing there and results stay deterministic. No money values:
//! whether those are exported at all is the binary's choice.

/// Counter: market events processed.
pub const EVENTS: &str = "tradebot_events_total";
/// Gauge, per `instrument`: close time of the latest candle, Unix seconds.
/// `time() - this` is the feed's age; alert when it exceeds a few bars.
pub const LAST_CANDLE: &str = "tradebot_last_candle_close_seconds";
/// Counter, per `instrument` and `side`: orders the broker accepted.
pub const ORDERS: &str = "tradebot_orders_submitted_total";
/// Counter: cancels the broker accepted.
pub const CANCELS: &str = "tradebot_cancels_requested_total";
/// Counter, per `instrument`, `side` and `liquidity`: fills.
pub const FILLS: &str = "tradebot_fills_total";
/// Counter, per `source` (`risk` or `broker`) and `code` (a Binance error
/// code, or the broker's refusal kind): signals that didn't become orders.
pub const REJECTED: &str = "tradebot_signals_rejected_total";
