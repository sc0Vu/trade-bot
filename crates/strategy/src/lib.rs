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
pub use params::{Params, exact_decimal};
pub use registry::{StrategyRegistration, build, names};

use bot_core::{Candle, Fill, MarketEvent, Order, Signal};

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

    /// The strategy's own state (phase, baselines, flags) as a value a live
    /// run saves after every event, so a restart can carry on where it
    /// stopped. Include a format version so a later build can refuse what
    /// it doesn't understand. Money stays in decimal strings (invariant 1).
    /// `None` (the default): nothing to save.
    fn save_state(&self) -> Option<serde_json::Value> {
        None
    }

    /// Takes back what [`save_state`](Self::save_state) returned, after
    /// [`on_start`](Self::on_start) and before the first event. `ctx` shows
    /// the books as they are now (positions, open orders): state that
    /// contradicts them (a run that crashed between a fill and the next
    /// save) must be refused, as must another format version. All or
    /// nothing: on `Err` the strategy must be as `on_start` left it, and it
    /// runs fresh.
    fn restore_state(
        &mut self,
        _state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        Ok(())
    }

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

    /// Called when the broker accepts an order from one of this strategy's
    /// signals, in signal order, at the end of the event that emitted it:
    /// before any fill of it can arrive (fills come from later settles), so
    /// a strategy keeping several orders can learn each one's id and know
    /// whose a fill is. An order can fill before it is ever seen in
    /// `open_orders`. Signals submitted here are routed with the next
    /// event's.
    fn on_order(&mut self, _ctx: &mut Context, _order: &Order) {}

    /// Called when a signal (submit or cancel) is refused, by risk or by
    /// the venue refusing that one order (`-2010` insufficient balance, a
    /// filter), with the reason. Not called for errors that stop the run.
    /// Signals submitted here are routed with the next event's.
    fn on_rejected(&mut self, _ctx: &mut Context, _signal: &Signal, _reason: &str) {}
}
