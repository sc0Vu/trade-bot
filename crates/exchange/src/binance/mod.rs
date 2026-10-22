//! Binance spot. [`public`] reads market data that needs no API key;
//! [`signed`] holds credentials, request signing and the HTTP transport
//! used for account endpoints; [`ws`] streams closed klines.

mod broker;
mod provider;
pub mod public;
pub mod signed;
pub mod ws;

pub use broker::BinanceBroker;
pub use provider::{BinanceConfig, BinanceSpotProvider};
pub use public::PublicClient;
pub use signed::{
    Clock, Credentials, HttpRequest, HttpResponse, Method, ReqwestTransport, SignedClient,
    SystemClock, Transport,
};
pub use ws::KlineStream;

/// Binance's market-data-only host: public endpoints, no key, separate
/// rate limits from trading. Real (mainnet) history, which is what a
/// backtest wants even when trading targets testnet.
pub const PUBLIC_DATA_URL: &str = "https://data-api.binance.vision";

/// Spot REST host for real trading. Reached only through the live gate.
pub const LIVE_REST_URL: &str = "https://api.binance.com";
/// Spot testnet REST host: fake balances, same API.
pub const TESTNET_REST_URL: &str = "https://testnet.binance.vision";
/// Spot market-data WebSocket host for real trading.
pub const LIVE_WS_URL: &str = "wss://stream.binance.com:9443";
/// Spot testnet market-data WebSocket host.
pub const TESTNET_WS_URL: &str = "wss://stream.testnet.binance.vision";
