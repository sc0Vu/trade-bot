//! Binance spot rate limits: what each request costs, and the limits it
//! counts against.
//!
//! Weights were measured on 2026-10-23 from the `X-MBX-USED-WEIGHT-1M`
//! header before and after each call (demo trading host); `POST` and
//! `DELETE /api/v3/order` were not measured (that would trade) and use
//! Binance's documented weight of 1. An endpoint not listed costs 20, the
//! largest weight the bot otherwise pays. Binance's own count, reported on
//! every response, corrects any drift ([`crate::rate_limit::RateLimiter::observe_used`]).

use super::signed::Method;
use crate::rate_limit::{Kind, Limit};
use serde_json::Value;
use std::time::Duration;

/// Share of each Binance limit the bot allows itself, leaving room for
/// other programs on the same IP or account.
pub const HEADROOM: f64 = 0.8;

const MINUTE: Duration = Duration::from_secs(60);
const TEN_SECONDS: Duration = Duration::from_secs(10);
const DAY: Duration = Duration::from_secs(86_400);

/// Window of the `X-MBX-USED-WEIGHT-1M` header.
pub const WEIGHT_WINDOW: Duration = MINUTE;
/// Window of the `X-MBX-ORDER-COUNT-10S` header.
pub const ORDER_WINDOW: Duration = TEN_SECONDS;

/// Request weight of one call. `query` matters where Binance's weight
/// depends on parameters (`myTrades` with or without `orderId`,
/// `openOrders` with or without `symbol`).
pub fn weight(method: Method, path: &str, query: &str) -> u64 {
    let has = |key: &str| query.split('&').any(|kv| kv.split('=').next() == Some(key));
    match (method, path) {
        (_, "/api/v3/time" | "/api/v3/ping") => 1,
        (_, "/api/v3/klines") => 2,
        (Method::Get, "/api/v3/order") => 4,
        (Method::Post | Method::Delete, "/api/v3/order") => 1,
        (_, "/api/v3/myTrades") if has("orderId") => 5,
        (_, "/api/v3/openOrders") if has("symbol") => 6,
        (_, "/api/v3/openOrders") => 80,
        _ => 20,
    }
}

/// Orders one call places: 1 for a new order, else 0.
pub fn orders(method: Method, path: &str) -> u64 {
    u64::from(method == Method::Post && path == "/api/v3/order")
}

/// Binance's published spot limits as of 2026-10-23, with [`HEADROOM`]:
/// request weight 6000 per minute, orders 100 per 10 s and 200,000 per day.
pub fn default_limits() -> Vec<Limit> {
    vec![
        Limit::with_headroom(Kind::Weight, MINUTE, 6000, HEADROOM),
        Limit::with_headroom(Kind::Orders, TEN_SECONDS, 100, HEADROOM),
        Limit::with_headroom(Kind::Orders, DAY, 200_000, HEADROOM),
    ]
}

/// The limits in an `exchangeInfo` reply's `rateLimits`, with
/// [`HEADROOM`]. `REQUEST_WEIGHT` and `ORDERS` entries are used;
/// `RAW_REQUESTS` (hundreds of thousands per 5 minutes) is out of reach at
/// this bot's rate and skipped. `None` if the reply has no usable entry,
/// so the caller keeps [`default_limits`].
pub fn parse_limits(info: &Value) -> Option<Vec<Limit>> {
    let limits: Vec<Limit> = info["rateLimits"]
        .as_array()?
        .iter()
        .filter_map(|l| {
            let kind = match l["rateLimitType"].as_str()? {
                "REQUEST_WEIGHT" => Kind::Weight,
                "ORDERS" => Kind::Orders,
                _ => return None,
            };
            let unit = match l["interval"].as_str()? {
                "SECOND" => 1,
                "MINUTE" => 60,
                "HOUR" => 3_600,
                "DAY" => 86_400,
                _ => return None,
            };
            let n = l["intervalNum"].as_u64()?;
            let limit = l["limit"].as_u64()?;
            Some(Limit::with_headroom(
                kind,
                Duration::from_secs(unit * n),
                limit,
                HEADROOM,
            ))
        })
        .collect();
    (!limits.is_empty()).then_some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The weights measured on demo trading, 2026-10-23.
    #[test]
    fn weights_match_what_binance_charged() {
        let g = Method::Get;
        assert_eq!(weight(g, "/api/v3/time", ""), 1);
        assert_eq!(weight(g, "/api/v3/exchangeInfo", "symbol=BTCUSDT"), 20);
        assert_eq!(weight(g, "/api/v3/klines", "symbol=BTCUSDT&interval=1m"), 2);
        assert_eq!(weight(g, "/api/v3/account", "omitZeroBalances=true"), 20);
        assert_eq!(weight(g, "/api/v3/openOrders", "symbol=BTCUSDT"), 6);
        assert_eq!(weight(g, "/api/v3/myTrades", "symbol=BTCUSDT&limit=1"), 20);
        assert_eq!(weight(g, "/api/v3/myTrades", "symbol=BTCUSDT&orderId=7"), 5);
        assert_eq!(weight(g, "/api/v3/order", "symbol=BTCUSDT&orderId=7"), 4);
        assert_eq!(weight(g, "/api/v3/allOrders", "symbol=BTCUSDT"), 20);
        // Documented, not measured.
        assert_eq!(weight(Method::Post, "/api/v3/order", ""), 1);
        assert_eq!(weight(Method::Delete, "/api/v3/order", ""), 1);
        assert_eq!(weight(g, "/api/v3/openOrders", ""), 80);
        // "orderIdX" is not "orderId".
        assert_eq!(weight(g, "/api/v3/myTrades", "orderIdX=1"), 20);
    }

    #[test]
    fn only_a_new_order_counts_as_an_order() {
        assert_eq!(orders(Method::Post, "/api/v3/order"), 1);
        assert_eq!(orders(Method::Delete, "/api/v3/order"), 0);
        assert_eq!(orders(Method::Get, "/api/v3/order"), 0);
    }

    #[test]
    fn defaults_keep_twenty_percent_headroom() {
        let d = default_limits();
        assert_eq!(d[0], Limit::with_headroom(Kind::Weight, MINUTE, 6000, 0.8));
        assert_eq!(d[0].budget, 4800);
        assert_eq!(d[1].budget, 80);
        assert_eq!(d[2].budget, 160_000);
    }

    /// The `rateLimits` demo trading returned on 2026-10-23.
    #[test]
    fn parses_exchange_info_rate_limits() {
        let info: Value = serde_json::from_str(
            r#"{"rateLimits":[
              {"rateLimitType":"REQUEST_WEIGHT","interval":"MINUTE","intervalNum":1,"limit":6000},
              {"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":100},
              {"rateLimitType":"ORDERS","interval":"DAY","intervalNum":1,"limit":200000},
              {"rateLimitType":"RAW_REQUESTS","interval":"MINUTE","intervalNum":5,"limit":300000}]}"#,
        )
        .unwrap();
        assert_eq!(parse_limits(&info), Some(default_limits()));
        assert_eq!(parse_limits(&serde_json::json!({})), None);
        assert_eq!(parse_limits(&serde_json::json!({"rateLimits": []})), None);
    }
}
