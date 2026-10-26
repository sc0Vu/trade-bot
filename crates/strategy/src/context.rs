use crate::Params;
use bot_core::{Candle, Instrument, InstrumentId, Order, Position, Signal, Timestamp};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// A strategy's only window to the world (invariant 5): the trading
/// universe, positions, open orders, rolling candle history, params, and a
/// way to submit signals. It holds no broker, provider, or network handle.
///
/// History holds data up to and including the current event and nothing
/// later (invariant 2): the engine adds each event before calling the
/// strategy, and there is no way to reach ahead. Positions are the broker's,
/// copied in by the engine after each fill.
///
/// The engine updates a context through [`driver`]. Those functions are
/// public because the engine is another crate; a strategy calling them only
/// corrupts its own view, since the broker remains the source of truth.
#[derive(Debug)]
pub struct Context {
    now: Option<Timestamp>,
    instruments: BTreeMap<InstrumentId, Instrument>,
    positions: HashMap<InstrumentId, Position>,
    open_orders: Vec<Order>,
    cancelling: Vec<Order>,
    history: HashMap<InstrumentId, VecDeque<Candle>>,
    history_len: usize,
    params: Params,
    outbox: Vec<Signal>,
}

impl Context {
    /// Time of the current event; `None` before the first one (`on_start`).
    pub fn now(&self) -> Option<Timestamp> {
        self.now
    }

    /// Reference data for an instrument in the universe, for asset-class math
    /// (notional, rounding) that must not be redone by hand (invariant 4).
    pub fn instrument(&self, id: &InstrumentId) -> Option<&Instrument> {
        self.instruments.get(id)
    }

    /// The tradable universe (the whitelist), ordered by id.
    pub fn instruments(&self) -> impl Iterator<Item = &Instrument> {
        self.instruments.values()
    }

    /// Current position; flat if never traded.
    pub fn position(&self, id: &InstrumentId) -> Position {
        self.positions
            .get(id)
            .cloned()
            .unwrap_or_else(|| Position::flat(id.clone()))
    }

    /// The strategy's orders still open on `id`, as of the broker's last
    /// settle (before this event's signals were routed). Their ids are what
    /// `Signal::Cancel` takes.
    pub fn open_orders(&self, id: &InstrumentId) -> impl Iterator<Item = &Order> {
        self.open_orders.iter().filter(move |o| o.instrument == *id)
    }

    /// The strategy's orders on `id` it has cancelled that may still fill
    /// (as of the last settle): a venue confirms a cancel, and reports any
    /// fill that beat it, only later. Until this and `open_orders` are both
    /// empty, a fill on `id` can still arrive. Never cancel these again.
    pub fn cancelling_orders(&self, id: &InstrumentId) -> impl Iterator<Item = &Order> {
        self.cancelling.iter().filter(move |o| o.instrument == *id)
    }

    /// Closed candles for `id`, oldest first, ending with the current one.
    /// At most `history_len` long.
    pub fn history(
        &self,
        id: &InstrumentId,
    ) -> impl DoubleEndedIterator<Item = &Candle> + ExactSizeIterator {
        self.history.get(id).map(VecDeque::iter).unwrap_or_default()
    }

    /// Most recent closed candle for `id`.
    pub fn last_candle(&self, id: &InstrumentId) -> Option<&Candle> {
        self.history.get(id).and_then(VecDeque::back)
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Queues a signal for risk. Use from `on_start` and `on_fill`; signals
    /// returned from `on_event` are queued after these.
    pub fn submit(&mut self, signal: Signal) {
        self.outbox.push(signal);
    }
}

/// Engine-side operations on a [`Context`]. Not for strategies.
pub mod driver {
    use super::*;

    /// A context over `instruments` (the resolved whitelist) keeping the last
    /// `history_len` candles per instrument (at least 1).
    pub fn new(instruments: Vec<Instrument>, params: Params, history_len: usize) -> Context {
        Context {
            now: None,
            instruments: instruments.into_iter().map(|i| (i.id.clone(), i)).collect(),
            positions: HashMap::new(),
            open_orders: Vec::new(),
            cancelling: Vec::new(),
            history: HashMap::new(),
            history_len: history_len.max(1),
            params,
            outbox: Vec::new(),
        }
    }

    pub fn set_now(ctx: &mut Context, ts: Timestamp) {
        ctx.now = Some(ts);
    }

    /// Appends a closed candle, evicting the oldest past `history_len`.
    /// Candles outside the universe are ignored.
    pub fn push_candle(ctx: &mut Context, candle: Candle) {
        if !ctx.instruments.contains_key(&candle.instrument) {
            return;
        }
        let len = ctx.history_len;
        let h = ctx.history.entry(candle.instrument.clone()).or_default();
        h.push_back(candle);
        while h.len() > len {
            h.pop_front();
        }
    }

    /// Replaces the strategy's view of one position with the broker's.
    pub fn set_position(ctx: &mut Context, position: Position) {
        ctx.positions
            .insert(position.instrument().clone(), position);
    }

    /// Replaces the strategy's view of open orders with the broker's.
    pub fn set_open_orders(ctx: &mut Context, orders: Vec<Order>) {
        ctx.open_orders = orders;
    }

    /// Replaces the strategy's view of orders being cancelled.
    pub fn set_cancelling_orders(ctx: &mut Context, orders: Vec<Order>) {
        ctx.cancelling = orders;
    }

    /// Drains queued signals in submission order.
    pub fn take_signals(ctx: &mut Context) -> Vec<Signal> {
        std::mem::take(&mut ctx.outbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};

    #[test]
    fn history_keeps_last_n_oldest_first() {
        let mut ctx = driver::new(vec![btcusdt()], Params::default(), 2);
        for i in 0..3 {
            driver::push_candle(&mut ctx, candle(i, "100"));
        }
        let id = btcusdt().id.clone();
        let opens: Vec<i64> = ctx.history(&id).map(|c| c.open_time.as_millis()).collect();
        assert_eq!(opens, vec![3_600_000, 7_200_000]);
        assert_eq!(
            ctx.last_candle(&id).unwrap().open_time.as_millis(),
            7_200_000
        );
    }

    #[test]
    fn candles_outside_universe_are_ignored() {
        let mut ctx = driver::new(vec![], Params::default(), 10);
        driver::push_candle(&mut ctx, candle(0, "100"));
        assert_eq!(ctx.history(&btcusdt().id.clone()).len(), 0);
    }

    #[test]
    fn position_defaults_to_flat() {
        let ctx = driver::new(vec![btcusdt()], Params::default(), 1);
        assert!(ctx.position(&btcusdt().id.clone()).is_flat());
    }
}
