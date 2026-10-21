//! The `Strategy` trait, the `Context` that is a strategy's only window to
//! the world, the `inventory` registry, and built-in strategies (`ratio_hop`).
//!
//! Strategies are provider-agnostic: no handle to a broker, provider, or
//! network, so every strategy runs unchanged in backtest and live.
