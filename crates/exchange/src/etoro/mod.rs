//! eToro (Public API, `https://public-api.etoro.com`): a broker, not an
//! order book. Docs read 2026-11-18 (api-portal.etoro.com, OpenAPI
//! v1.417.0); shapes below checked against the demo account the same day.
//!
//! - **Auth**: every request carries `x-api-key` (the application's public
//!   key, env `ETORO_PUBLIC_KEY`), `x-user-key` (the account's user key,
//!   env `ETORO_PRIVATE_KEY`) and a fresh `x-request-id` (UUID; it is also
//!   the idempotency key of an order). A user key belongs to one account,
//!   demo or real: a demo key gets `403 InsufficientPermissions` on real
//!   paths. Requests need a non-default `User-Agent` (Cloudflare answers a
//!   bare client with `403`, "error code: 1010").
//! - **Demo vs real**: same host; trading and portfolio paths carry
//!   `/demo/` ([`Account::path`]).
//! - **Instruments** ([`market`]): `etoro:<SYMBOL>` (eToro's `symbol`,
//!   matched exactly; its search endpoint matches by prefix). Traded here
//!   as the real asset (`settlementType real`, long only, leverage 1) =
//!   [`AssetClass::Spot`](bot_core::AssetClass::Spot): owned inventory.
//!   eToro publishes no tick or unit step: tick 1e-8, lot 1e-8 for
//!   fractional units (1 for whole), minimum notional =
//!   `minPositionExposure` (eligibility).
//! - **Prices**: most REST money fields are JSON numbers, read from the
//!   text, never `f64` (invariant 1); the date-range candles carry strings.
//! - **Rate limits** (per user key, 60 s rolling): market data 120,
//!   trading execution 20, portfolio info 60, else 60; 80% budgeted.

pub mod broker;
pub mod client;
pub mod feed;
pub mod market;

pub use client::{Account, EtoroClient, EtoroKeys};

/// Venue id of eToro instruments.
pub const VENUE: &str = "etoro";
/// The one REST host (demo and real alike).
pub const REST_URL: &str = "https://public-api.etoro.com";
