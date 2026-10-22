//! The `Strategy` trait, the `Context` that is a strategy's only window to
//! the world (invariant 5), the `inventory` registry, and built-in
//! strategies.
//!
//! Strategies are provider-agnostic: no handle to a broker, provider, or
//! network, so every strategy runs unchanged in backtest and live.

pub mod builtin;
mod context;
mod error;
mod params;
mod registry;

pub use context::{Context, driver};
pub use error::StrategyError;
pub use params::Params;
pub use registry::{StrategyRegistration, build, names};

use bot_core::{Candle, Fill, MarketEvent, Signal};

/// A trading strategy: a pure decision function from market events to
/// signals.
///
/// Everything it may read or do goes through [`Context`]. It never sees a
/// broker, provider, or socket, so the same code runs in backtest and live
/// (invariant 3) and the import mechanism can later move to scripts or wasm
/// without touching strategies (invariant 5).
///
/// Signals are intents, not orders: risk sizes, checks and may reject them.
/// An order decided on event t fills no earlier than event t+1.
pub trait Strategy: Send {
    /// Registry name, as selected by `strategy.name` in config.
    fn name(&self) -> &str;

    /// Called once before the first event. Signals go through
    /// [`Context::submit`].
    fn on_start(&mut self, _ctx: &mut Context) {}

    /// Called for every event, after fills caused by it have been delivered
    /// and after the event has been added to the rolling history. The default
    /// sends candles to [`on_candle`](Self::on_candle) and ignores the rest.
    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> {
        match e {
            MarketEvent::Candle(c) => self.on_candle(ctx, c),
            _ => Vec::new(),
        }
    }

    /// Convenience hook for candle-only strategies.
    fn on_candle(&mut self, _ctx: &mut Context, _c: &Candle) -> Vec<Signal> {
        Vec::new()
    }

    /// Called for each fill, before the event that caused it is added to the
    /// history: a fill at a candle's open must not see that candle's close.
    /// Signals go through [`Context::submit`].
    fn on_fill(&mut self, _ctx: &mut Context, _f: &Fill) {}
}
