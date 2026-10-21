//! Domain types shared by every crate: money newtypes (`Price`, `Qty`,
//! `Notional`), identifiers, `Instrument`, `Candle`, `MarketEvent`, `Signal`,
//! `Order`, `Fill`, and `Position`.
//!
//! Asset-class math (notional, settlement, rounding to venue filters) lives
//! here on `Instrument`, never in a broker or strategy. Money is never a float.

mod error;
mod money;

pub use error::Error;
pub use money::{Notional, Price, Qty};

/// Re-exported so callers build money values from the same `Decimal` version.
pub use rust_decimal::Decimal;
