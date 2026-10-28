//! MAX (MaiCoin, Taiwan) spot. [`public`] reads market data that needs no
//! API key; [`ws`] streams closed klines.
//!
//! MAX has no testnet or sandbox: its only host trades real balances. So a
//! fake-money run on MAX can't send orders anywhere; it pairs MAX's live
//! market data with the local paper broker.
//!
//! Precision is per market, not per coin: BTC steps by 0.00000001 on
//! `btctwd` but by 0.000001 on `btcusdt`. A market's `base_unit_precision`
//! and `quote_unit_precision` are decimal places, and may be negative (a
//! precision of -3 steps by 1000), see [`public::step`].

pub mod paper;
pub mod public;
pub mod ws;

use crate::rate_limit::{Kind, Limit};
use std::time::Duration;

/// MAX's REST host (production; there is no other).
pub const REST_URL: &str = "https://max-api.maicoin.com";
/// MAX's WebSocket host.
pub const WS_URL: &str = "wss://max-stream.maicoin.com/ws";
/// Venue id of MAX instruments (`max:BTCUSDT`).
pub const VENUE: &str = "max";

/// Share of each MAX limit the bot allows itself.
pub const HEADROOM: f64 = 0.8;

/// MAX's published REST limit (API document, checked 2026-10-28): 1200
/// requests a minute per IP for public endpoints and per account for
/// private ones, every request counting 1, with [`HEADROOM`].
pub fn default_limits() -> Vec<Limit> {
    vec![Limit::with_headroom(
        Kind::Weight,
        Duration::from_secs(60),
        1200,
        HEADROOM,
    )]
}
