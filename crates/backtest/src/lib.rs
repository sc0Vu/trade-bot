//! Backtesting: cached kline loader, `BacktestProvider` (kline replay plus
//! paper broker), runner, and report.
//!
//! Replays the same `MarketEvent` stream live produces, through the shared
//! engine. The fill model's assumptions are documented, not hidden; backtest
//! P&L is model output, never a promise.

mod data;
mod feed;

pub use data::{DataDir, DataError, FetchSummary, fetch};
pub use feed::KlineFeed;

#[cfg(test)]
pub(crate) mod test_support;
