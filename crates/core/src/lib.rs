//! Domain types shared by every crate: money newtypes (`Price`, `Qty`,
//! `Notional`), identifiers, `Instrument`, `Candle`, `MarketEvent`, `Signal`,
//! `Order`, `Fill`, and `Position`.
//!
//! Asset-class math (notional, settlement, rounding to venue filters) lives
//! here on `Instrument`, never in a broker or strategy. Money is never a float.
//!
//! Validated types (`Instrument`, `Candle`, `Quote`, `Order`, `Fill`) follow
//! one pattern: a `*Spec` struct with public fields holds unchecked input, and
//! the type itself can only be built from a spec through a checking `new`
//! (or `try_from` when deserialized). Fields are read through `Deref` to the
//! spec, so a value that exists has passed validation.

mod error;
mod ids;
mod instrument;
mod market;
mod money;
mod order;
mod time;

pub use error::{Error, FilterError};
pub use ids::{Asset, InstrumentId, Symbol, VenueId};
pub use instrument::{AssetClass, Instrument, InstrumentSpec, Settlement};
pub use market::{Candle, CandleSpec, CorpAction, Interval, MarketEvent, Quote, QuoteSpec};
pub use money::{Notional, Price, Qty};
pub use order::Side;
pub use time::Timestamp;

/// Re-exported so callers build money values from the same `Decimal` version.
pub use rust_decimal::Decimal;
