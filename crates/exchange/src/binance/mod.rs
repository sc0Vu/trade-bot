//! Binance spot. [`public`] reads market data that needs no API key.

pub mod public;

pub use public::PublicClient;

/// Binance's market-data-only host: public endpoints, no key, separate
/// rate limits from trading. Real (mainnet) history, which is what a
/// backtest wants even when trading targets testnet.
pub const PUBLIC_DATA_URL: &str = "https://data-api.binance.vision";
