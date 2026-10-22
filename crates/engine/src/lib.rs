//! The single event loop shared by backtest and live.
//!
//! Per event: the broker settles pending orders against the new event
//! *before* the strategy runs, so an order decided on event t fills no
//! earlier than t+1. Only the provider differs between backtest and live
//! (invariant 3).

mod error;
mod event_loop;
pub mod metrics;
pub mod risk;

pub use error::EngineError;
pub use event_loop::{EngineConfig, Observer, Rejection, RunReport, Snapshot, run, run_observed};

#[cfg(test)]
pub(crate) mod tests_support;
