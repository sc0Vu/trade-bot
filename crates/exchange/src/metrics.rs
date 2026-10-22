//! Metric names the venue client records, through the `metrics` facade.
//! Nothing is recorded unless the binary installs a recorder. Labels are
//! bounded: endpoint paths and error codes, never ids or amounts.

/// Histogram, per `method` and `path`: Binance REST request time, seconds.
pub const BINANCE_REQUEST: &str = "tradebot_binance_request_seconds";
/// Counter, per `path` and `code` (a Binance error code, or `http` for a
/// transport failure or non-JSON error): failed Binance REST requests.
pub const BINANCE_ERRORS: &str = "tradebot_binance_errors_total";
/// Counter, per `phase` (`startup`, `exit`): journal orders reconcile
/// closed.
pub const RECONCILE_ORDERS: &str = "tradebot_reconcile_orders_closed_total";
/// Counter, per `phase`: fills reconcile recorded that the books lacked.
pub const RECONCILE_FILLS: &str = "tradebot_reconcile_fills_recovered_total";
