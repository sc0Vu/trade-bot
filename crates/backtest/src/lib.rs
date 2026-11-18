//! Backtesting: cached kline loader, `BacktestProvider` (kline replay plus
//! paper broker), runner, and report.
//!
//! Replays the same `MarketEvent` stream live produces, through the shared
//! engine. The fill model's assumptions are documented, not hidden; backtest
//! P&L is model output, never a promise.

mod data;
mod feed;
mod listing;
mod lookahead;
mod perp;
mod provider;
mod quality;
mod report;
mod runner;
mod split;

pub use data::{DataDir, DataError, FetchSummary, HistorySource, fetch};
pub use feed::KlineFeed;
pub use listing::{Delisting, cut_at_gaps, read_delistings};
pub use lookahead::{BuildStrategy, LookaheadError, LookaheadReport, check_lookahead};
pub use perp::{FundingRate, PerpData, fetch_funding, fetch_marks, funding_events, mark_events};
pub use provider::{BacktestProvider, HedgeSetup, PaperConfig, PerpSetup};
pub use quality::{SuspectMove, suspect_moves};
pub use report::{EarnModel, Report};
pub use runner::{BacktestError, run_backtest};

#[cfg(test)]
pub(crate) mod test_support;
