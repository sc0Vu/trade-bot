//! Backtesting: cached kline loader, `BacktestProvider` (kline replay plus
//! paper broker), runner, and report.
//!
//! Replays the same `MarketEvent` stream live produces, through the shared
//! engine. The fill model's assumptions are documented, not hidden; backtest
//! P&L is model output, never a promise.

mod data;
mod feed;
mod provider;
mod report;
mod runner;

pub use data::{DataDir, DataError, FetchSummary, HistorySource, fetch};
pub use feed::KlineFeed;
pub use provider::{BacktestProvider, PaperConfig};
pub use report::Report;
pub use runner::{BacktestError, run_backtest};

#[cfg(test)]
pub(crate) mod test_support;
