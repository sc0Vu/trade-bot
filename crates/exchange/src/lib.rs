//! Venue boundary: the [`Provider`], [`MarketData`], and [`Broker`] traits,
//! and [`paper::PaperBroker`], the simulated broker used by backtest and
//! paper trading. The Binance provider will live here too.
//!
//! A broker submits orders and reports fills; it never values an instrument
//! (invariant 4). How a simulated order fills is a pluggable
//! [`paper::FillModel`], so backtest and live differ only by which fill
//! model is plugged in; live's "model" is the real exchange.

mod error;
pub mod paper;
mod traits;

pub use error::ExchangeError;
pub use traits::{Broker, InstrumentQuery, MarketData, Provider};
