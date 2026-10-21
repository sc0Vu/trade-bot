//! Domain types shared by every crate: `Instrument`, `AssetClass`, `Candle`,
//! `MarketEvent`, `Order`, `Signal`, `Position`, and money as `Decimal`.
//!
//! Asset-class math (notional, settlement, multiplier) lives here on
//! `Instrument`, never in a broker or strategy. Money is never a float.
