//! The no-lookahead property (invariant 2) as a check any strategy can be
//! put through, not only the built-ins the tests name.
//!
//! For each cut point t (up to `cuts` close times spread over the series)
//! every candle opening after t's close is rewritten: halved on the even
//! instruments (in the order given), doubled on the odd ones. A run on the
//! rewritten feed must make exactly the decisions the original run made up
//! to t's close (every order accepted and every signal refused, with its
//! time) and the same fills up to then: they come from candles up to t, so
//! nothing later may change them. Decisions are compared, not only fills:
//! a strategy peeking one bar ahead changes an order decided at t, which
//! fills after t, where a fills-only comparison wouldn't look. A strategy,
//! engine or broker that reads ahead breaks it.
//!
//! A pass is meaningful only if the rewrite changed something later
//! (`future_mattered`): a strategy that never trades passes vacuously.
//! Runs use the default fill model on a spot paper broker; the cost is one
//! full backtest per cut.

use crate::{BacktestProvider, PaperConfig};
use bot_core::{
    Candle, CandleSpec, Decimal, Fill, Instrument, MarketEvent, Order, Signal, Timestamp,
};
use engine::{EngineConfig, EngineError, RunReport};
use std::sync::{Arc, Mutex};
use strategy::{Context, Params, Strategy, StrategyError};
use thiserror::Error;

/// Builds a fresh strategy for one run, given that run's feed (module
/// docs: only a test uses the feed).
pub type BuildStrategy<'a> = dyn Fn(&[Candle]) -> Result<Box<dyn Strategy>, StrategyError> + 'a;

/// A decision the strategy made, as the check compares them: when, and
/// what (the accepted order or the refused signal, as text).
type Decision = (Timestamp, String);

/// Wraps the strategy under test, logging its decisions; forwards every
/// hook unchanged.
struct Recorder {
    inner: Box<dyn Strategy>,
    log: Arc<Mutex<Vec<Decision>>>,
}

impl Recorder {
    fn note(&self, ctx: &Context, what: String) {
        let at = ctx.now().unwrap_or(Timestamp::from_millis(0));
        if let Ok(mut l) = self.log.lock() {
            l.push((at, what));
        }
    }
}

impl Strategy for Recorder {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn on_start(&mut self, ctx: &mut Context) {
        self.inner.on_start(ctx);
    }
    fn save_state(&self) -> Option<serde_json::Value> {
        self.inner.save_state()
    }
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        ctx: &Context,
    ) -> Result<(), StrategyError> {
        self.inner.restore_state(state, ctx)
    }
    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> {
        self.inner.on_event(ctx, e)
    }
    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        self.inner.on_candle(ctx, c)
    }
    fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
        self.inner.on_fill(ctx, f);
    }
    fn on_order(&mut self, ctx: &mut Context, order: &Order) {
        self.note(ctx, format!("order {order:?}"));
        self.inner.on_order(ctx, order);
    }
    fn on_rejected(&mut self, ctx: &mut Context, signal: &Signal, reason: &str) {
        self.note(ctx, format!("refused {signal:?}: {reason}"));
        self.inner.on_rejected(ctx, signal, reason);
    }
    fn fault(&self) -> Option<String> {
        self.inner.fault()
    }
}

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookaheadReport {
    /// Cut points checked.
    pub cuts: usize,
    /// Whether rewriting the future changed any later fill at all (else
    /// the pass says nothing: the strategy may not have traded).
    pub future_mattered: bool,
}

/// Why the check failed.
#[derive(Debug, Error)]
pub enum LookaheadError {
    /// Fills or rejections up to `cut` changed when only later candles did.
    #[error("{what} up to {cut} changed when only candles after it did: the strategy reads ahead")]
    Changed { cut: Timestamp, what: &'static str },
    #[error("no candles to check")]
    NoData,
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs the check (module docs) with a fresh strategy from `build` per
/// run. `build` is handed that run's feed only so a test can build a
/// strategy that cheats; a real builder ignores it.
pub async fn check_lookahead(
    instruments: &[Instrument],
    candles: &[Candle],
    paper: PaperConfig,
    build: &BuildStrategy<'_>,
    params: &Params,
    config: &EngineConfig,
    cuts: usize,
) -> Result<LookaheadReport, LookaheadError> {
    let mut closes: Vec<Timestamp> = candles.iter().map(|c| c.close_time).collect();
    closes.sort();
    closes.dedup();
    if closes.is_empty() || cuts == 0 {
        return Err(LookaheadError::NoData);
    }
    let run = |feed: Vec<Candle>| async move {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut s = Recorder {
            inner: build(&feed)?,
            log: log.clone(),
        };
        let provider = BacktestProvider::new(instruments.to_vec(), feed, paper);
        let report = engine::run(&provider, &mut s, params.clone(), config).await?;
        let decisions = log.lock().map(|l| l.clone()).unwrap_or_default();
        Ok::<(RunReport, Vec<Decision>), LookaheadError>((report, decisions))
    };
    let (base, base_decisions) = run(candles.to_vec()).await?;
    // Evenly spread, the last close excluded (nothing after it to rewrite).
    let usable = closes.len().saturating_sub(1).max(1);
    let n = cuts.min(usable);
    let points: Vec<Timestamp> = (0..n).map(|k| closes[k * usable / n]).collect();
    let mut future_mattered = false;
    for cut in &points {
        let (alt, alt_decisions) = run(rewrite_after(instruments, candles, *cut)).await?;
        let early = |d: &[Decision]| -> Vec<Decision> {
            d.iter().filter(|(t, _)| t <= cut).cloned().collect()
        };
        if early(&base_decisions) != early(&alt_decisions) {
            return Err(LookaheadError::Changed {
                cut: *cut,
                what: "decisions",
            });
        }
        let early_fills = |r: &RunReport| -> Vec<_> {
            r.fills.iter().filter(|f| f.ts <= *cut).cloned().collect()
        };
        if early_fills(&base) != early_fills(&alt) {
            return Err(LookaheadError::Changed {
                cut: *cut,
                what: "fills",
            });
        }
        let early_rejections = |r: &RunReport| -> Vec<_> {
            r.rejections
                .iter()
                .filter(|x| x.at.is_none_or(|t| t <= *cut))
                .cloned()
                .collect()
        };
        if early_rejections(&base) != early_rejections(&alt) {
            return Err(LookaheadError::Changed {
                cut: *cut,
                what: "rejections",
            });
        }
        future_mattered |= base.fills != alt.fills;
    }
    Ok(LookaheadReport {
        cuts: points.len(),
        future_mattered,
    })
}

/// `candles` with every bar opening after `cut` scaled: × 0.5 on the even
/// instruments of `instruments`, × 2 on the odd ones.
fn rewrite_after(instruments: &[Instrument], candles: &[Candle], cut: Timestamp) -> Vec<Candle> {
    candles
        .iter()
        .map(|c| {
            if c.open_time <= cut {
                return c.clone();
            }
            let odd = instruments
                .iter()
                .position(|i| i.id == c.instrument)
                .is_some_and(|k| k % 2 == 1);
            let f = if odd {
                Decimal::TWO
            } else {
                Decimal::new(5, 1)
            };
            let s: CandleSpec = c.clone().into();
            Candle::new(CandleSpec {
                open: s.open * f,
                high: s.high * f,
                low: s.low * f,
                close: s.close * f,
                quote_volume: s.quote_volume * f,
                ..s
            })
            .unwrap_or_else(|_| c.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};
    use bot_core::{Notional, OrderKind, Qty, Side, Size};
    use exchange::paper::{BpsSlippage, RateFee};
    use rust_decimal_macros::dec;

    fn paper() -> PaperConfig {
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(100000)),
        }
    }

    fn buy(c: &Candle) -> Signal {
        Signal::Submit {
            instrument: c.instrument.clone(),
            side: Side::Buy,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
        }
    }

    /// Buys 1 whenever the close beats the previous close: honest.
    struct Honest;
    impl Strategy for Honest {
        fn name(&self) -> &str {
            "honest"
        }
        fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
            let prev = ctx.history(&c.instrument).rev().nth(1).map(|p| p.close);
            if prev.is_some_and(|p| c.close > p) {
                vec![buy(c)]
            } else {
                vec![]
            }
        }
    }

    /// Cheats by one bar: buys when the *next* bar (read from a copy of the
    /// run's feed) closes higher.
    struct Peeker(Vec<Candle>);
    impl Strategy for Peeker {
        fn name(&self) -> &str {
            "peeker"
        }
        fn on_candle(&mut self, _: &mut Context, c: &Candle) -> Vec<Signal> {
            let next = self.0.iter().find(|n| n.open_time > c.open_time);
            if next.is_some_and(|n| n.close > c.close) {
                vec![buy(c)]
            } else {
                vec![]
            }
        }
    }

    /// Never trades.
    struct Idle;
    impl Strategy for Idle {
        fn name(&self) -> &str {
            "idle"
        }
    }

    fn bars() -> Vec<Candle> {
        let id = btcusdt().id.clone();
        ["100", "101", "103", "102", "104", "106", "105", "107"]
            .iter()
            .enumerate()
            .map(|(i, p)| candle(&id, i as i64, [p, p, p, p]))
            .collect()
    }

    fn config() -> EngineConfig {
        EngineConfig {
            whitelist: vec![btcusdt().id.clone()],
            history_len: 10,
            ..Default::default()
        }
    }

    async fn check(build: &BuildStrategy<'_>) -> Result<LookaheadReport, LookaheadError> {
        check_lookahead(
            &[btcusdt()],
            &bars(),
            paper(),
            build,
            &Params::default(),
            &config(),
            7,
        )
        .await
    }

    #[tokio::test]
    async fn an_honest_strategy_passes_and_the_future_mattered() {
        let r = check(&|_| Ok(Box::new(Honest))).await.unwrap();
        assert_eq!(r.cuts, 7);
        assert!(r.future_mattered);
    }

    /// 100 → 101 at bar 1: the peeker buys at bar 0 in the original. Cut
    /// at bar 0's close: bar 1 is halved (50.5 < 100), so on the rewritten
    /// feed it doesn't buy at bar 0, a decision at the cut that changed.
    /// Its fill (bar 1's open) is after the cut: a fills-only check misses
    /// it.
    #[tokio::test]
    async fn a_one_bar_peek_is_caught_by_its_decisions() {
        let err = check(&|feed| Ok(Box::new(Peeker(feed.to_vec()))))
            .await
            .unwrap_err();
        assert!(
            matches!(err, LookaheadError::Changed { what: "decisions", cut } if cut.as_millis() == 3_599_999),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_strategy_that_never_trades_passes_vacuously() {
        let r = check(&|_| Ok(Box::new(Idle))).await.unwrap();
        assert!(!r.future_mattered);
    }
}
