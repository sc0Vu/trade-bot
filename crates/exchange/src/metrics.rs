//! Metric names the venue client records, through the `metrics` facade.
//! Nothing is recorded unless the binary installs a recorder. Labels are
//! bounded: endpoint paths and error codes, never ids or amounts.

/// Histogram, per `method` and `path`: Binance REST request time, seconds.
pub const BINANCE_REQUEST: &str = "tradebot_binance_request_seconds";
/// Counter, per `path` and `code` (a Binance error code, or `http` for a
/// transport failure or non-JSON error): failed Binance REST requests.
pub const BINANCE_ERRORS: &str = "tradebot_binance_errors_total";
/// Gauge: request weight spent in the last minute, by the rate limiter's
/// count (Binance's own reports included).
pub const BINANCE_WEIGHT_USED: &str = "tradebot_binance_weight_used";
/// Counter: requests the rate limiter made wait.
pub const RATE_LIMIT_WAITS: &str = "tradebot_rate_limit_waits_total";
/// Counter: 429/418 replies (Binance rate-limited or banned the IP).
pub const RATE_LIMITED: &str = "tradebot_binance_rate_limited_total";
/// Counter: times the live feed was lost and reconnected.
pub const FEED_RECONNECTS: &str = "tradebot_feed_reconnects_total";
/// Gauge: 1 while the user data stream is subscribed, else 0 (orders are
/// then all polled every event).
pub const USER_STREAM_UP: &str = "tradebot_user_stream_up";
/// Counter: subscribed user data streams that ended (drop, silence,
/// terminated) and were reconnected.
pub const USER_STREAM_DROPS: &str = "tradebot_user_stream_drops_total";
/// Counter: candles delivered as history-only backfill (warm-up and gaps).
pub const FEED_BACKFILLED: &str = "tradebot_feed_backfilled_total";
/// Counter, per `phase` (`startup`, `exit`): journal orders reconcile
/// closed.
pub const RECONCILE_ORDERS: &str = "tradebot_reconcile_orders_closed_total";
/// Counter, per `phase`: fills reconcile recorded that the books lacked.
pub const RECONCILE_FILLS: &str = "tradebot_reconcile_fills_recovered_total";
