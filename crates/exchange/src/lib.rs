//! Venue boundary: the [`Provider`], [`MarketData`], and [`Broker`] traits,
//! and [`paper::PaperBroker`], the simulated broker used by backtest and
//! paper trading, and [`binance`], the Binance spot client (public
//! market data so far; the live provider will follow).
//!
//! A broker submits orders and reports fills; it never values an instrument
//! (invariant 4). How a simulated order fills is a pluggable
//! [`paper::FillModel`], so backtest and live differ only by which fill
//! model is plugged in; live's "model" is the real exchange.

pub mod binance;
mod error;
pub mod metrics;
pub mod paper;
pub mod rate_limit;
#[cfg(test)]
mod test_db;
mod traits;

pub use error::ExchangeError;
pub(crate) use error::check_open_count;
pub use traits::{Books, Broker, InstrumentQuery, MarketData, Provider};
