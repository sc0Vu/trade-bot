//! Venue boundary: the `Provider`, `MarketData`, and `Broker` traits, the
//! Binance spot provider (testnet and live, one code path), its websocket
//! feed, and the paper broker.
//!
//! A broker submits orders and reports fills; it never values an instrument.
