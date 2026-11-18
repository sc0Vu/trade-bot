//! The restart property as a check any strategy can be put through:
//! stopping a run and starting it again from its saved state must not
//! change what it does.
//!
//! The whole series runs once uninterrupted. Then, at up to `cuts` close
//! times spread over it where nothing of the strategy's is working: run
//! the candles up to the cut, keep the strategy's last saved state, build
//! a fresh strategy, restore it, and run the rest after a warm-up (the
//! earlier candles as `Backfill`, as a live feed does), against the same
//! broker (the books survive a restart). The two halves' fills together
//! must equal the uninterrupted run's, order ids aside.
//!
//! Only idle cuts are compared: a live restart's reconcile cancels working
//! orders, which genuinely changes what follows. A strategy that always
//! has an order working has no idle cut, and the check says so; one that
//! never holds anything at a cut says that too (a restart while flat
//! proves little about restoring state).

use crate::PaperConfig;
use crate::lookahead::BuildStrategy;
use async_trait::async_trait;
use bot_core::{
    AssetClass, Candle, Fill, Instrument, InstrumentId, MarketEvent, Order, OrderId, OrderSpec,
    Position, Timestamp,
};
use engine::{EngineConfig, EngineError, Observer, Snapshot};
use exchange::paper::{NextOpenFillModel, PaperBroker};
use exchange::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use std::sync::Arc;
use strategy::{Params, StrategyError};
use thiserror::Error;
use tokio::sync::Mutex;

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartReport {
    /// Idle cuts compared.
    pub cuts: usize,
    /// Of those, cuts where something was held (the ones that test
    /// restoring a position-dependent state).
    pub while_holding: usize,
}

/// Why the check failed.
#[derive(Debug, Error)]
pub enum RestartError {
    /// Restarting at `cut` changed the fills.
    #[error(
        "restarting after {cut} changed the fills: expected {expected} fills, got {got}; \
         first difference at #{at}"
    )]
    Changed {
        cut: Timestamp,
        expected: usize,
        got: usize,
        at: usize,
    },
    /// Every candidate cut had an order working.
    #[error("no idle cut point: an order of the strategy's was always working")]
    NoIdleCut,
    #[error("no candles to check")]
    NoData,
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs the check (module docs). `candles` in replay order (close time,
/// then instrument).
pub async fn check_restart(
    instruments: &[Instrument],
    candles: &[Candle],
    paper: PaperConfig,
    build: &BuildStrategy<'_>,
    params: &Params,
    config: &EngineConfig,
    cuts: usize,
) -> Result<RestartReport, RestartError> {
    let mut sorted = candles.to_vec();
    sorted.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
    let mut closes: Vec<Timestamp> = sorted.iter().map(|c| c.close_time).collect();
    closes.dedup();
    if closes.len() < 2 || cuts == 0 {
        return Err(RestartError::NoData);
    }
    let new_broker = || Arc::new(Mutex::new(paper_broker(instruments, paper)));
    let as_events = |cs: &[Candle]| -> Vec<MarketEvent> {
        cs.iter().cloned().map(MarketEvent::Candle).collect()
    };

    // Uninterrupted.
    let expected: Vec<String> = {
        let provider = Replay {
            instruments: instruments.to_vec(),
            events: as_events(&sorted),
            broker: Shared(new_broker(), ""),
        };
        let mut s = build(&sorted)?;
        engine::run(&provider, s.as_mut(), params.clone(), config)
            .await?
            .fills
            .iter()
            .map(essence)
            .collect()
    };

    let usable = closes.len() - 1;
    let n = cuts.min(usable);
    let mut compared = 0;
    let mut while_holding = 0;
    for k in 0..n {
        let cut = closes[k * usable / n];
        let split = sorted.partition_point(|c| c.close_time <= cut);
        let broker = new_broker();
        let first = Replay {
            instruments: instruments.to_vec(),
            events: as_events(&sorted[..split]),
            broker: Shared(broker.clone(), ""),
        };
        let mut s1 = build(&sorted)?;
        let mut last = LastState(None);
        let r1 = engine::run_observed(&first, s1.as_mut(), params.clone(), config, &mut last, None)
            .await?;
        let (idle, holding) = {
            let b = broker.lock().await;
            (
                b.open_orders().is_empty(),
                config.whitelist.iter().any(|id| !b.position(id).is_flat()),
            )
        };
        if !idle {
            continue; // a live restart would cancel it: not comparable
        }
        let mut events: Vec<MarketEvent> = sorted[..split]
            .iter()
            .cloned()
            .map(MarketEvent::Backfill)
            .collect();
        events.extend(as_events(&sorted[split..]));
        let second = Replay {
            instruments: instruments.to_vec(),
            events,
            broker: Shared(broker.clone(), "r2-"),
        };
        let mut s2 = build(&sorted)?;
        let r2 = engine::run_observed(
            &second,
            s2.as_mut(),
            params.clone(),
            config,
            &mut LastState(None),
            last.0,
        )
        .await?;
        let got: Vec<String> = r1.fills.iter().chain(&r2.fills).map(essence).collect();
        if got != expected {
            let at = got
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(expected.len()));
            return Err(RestartError::Changed {
                cut,
                expected: expected.len(),
                got: got.len(),
                at,
            });
        }
        compared += 1;
        while_holding += usize::from(holding);
    }
    if compared == 0 {
        return Err(RestartError::NoIdleCut);
    }
    Ok(RestartReport {
        cuts: compared,
        while_holding,
    })
}

fn paper_broker(instruments: &[Instrument], paper: PaperConfig) -> PaperBroker {
    let b = PaperBroker::new(
        instruments.to_vec(),
        Box::new(NextOpenFillModel),
        Box::new(paper.slippage),
        Box::new(paper.fees),
    );
    match instruments.first() {
        Some(i) => b.with_cash(i.quote.clone(), paper.initial_cash),
        None => b,
    }
}

/// A fill as it matters for P&L: everything but the order id.
fn essence(f: &Fill) -> String {
    format!(
        "{} {:?} {} @ {} fee {:?} {:?} t{}",
        f.instrument,
        f.side,
        f.qty,
        f.price,
        f.fee,
        f.liquidity,
        f.ts.as_millis()
    )
}

/// The paper broker both halves of a restarted run trade against. The
/// second half's order ids get a prefix: engine ids restart at o-1, as
/// live, where the run tag keeps them apart.
#[derive(Clone)]
struct Shared(Arc<Mutex<PaperBroker>>, &'static str);

#[async_trait]
impl Broker for Shared {
    async fn submit(&mut self, o: Order) -> Result<OrderId, ExchangeError> {
        let spec: OrderSpec = o.into();
        let id = OrderId::new(format!("{}{}", self.1, spec.id))
            .map_err(|e| ExchangeError::Decode(e.to_string()))?;
        let o = Order::new(OrderSpec { id, ..spec })
            .map_err(|e| ExchangeError::Decode(e.to_string()))?;
        self.0.lock().await.submit(o).await
    }
    async fn cancel(&mut self, id: OrderId) -> Result<(), ExchangeError> {
        self.0.lock().await.cancel(id).await
    }
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>, ExchangeError> {
        self.0.lock().await.settle(e).await
    }
    fn position(&self, i: &InstrumentId) -> Position {
        // Only ever called between awaits: the lock is free.
        self.0
            .try_lock()
            .map(|b| b.position(i))
            .unwrap_or_else(|_| Position::flat(i.clone()))
    }
    fn open_orders(&self) -> Vec<Order> {
        self.0
            .try_lock()
            .map(|b| b.open_orders())
            .unwrap_or_default()
    }
    fn cancelling_orders(&self) -> Vec<Order> {
        self.0
            .try_lock()
            .map(|b| b.cancelling_orders())
            .unwrap_or_default()
    }
}

struct Script(std::vec::IntoIter<MarketEvent>);

#[async_trait]
impl MarketData for Script {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        Ok(self.0.next())
    }
}

struct Replay {
    instruments: Vec<Instrument>,
    events: Vec<MarketEvent>,
    broker: Shared,
}

#[async_trait]
impl Provider for Replay {
    fn id(&self) -> &str {
        "replay"
    }
    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }
    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(Script(self.events.clone().into_iter())))
    }
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        Ok(Box::new(self.broker.clone()))
    }
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = q;
        Ok(ids
            .iter()
            .filter_map(|id| self.instruments.iter().find(|i| i.id == *id).cloned())
            .collect())
    }
}

/// Keeps the last state the strategy saved.
struct LastState(Option<serde_json::Value>);

impl Observer for LastState {
    fn on_snapshot(&mut self, s: &Snapshot) {
        self.0 = s.strategy_state.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};
    use bot_core::{Notional, OrderKind, Qty, Side, Signal, Size};
    use exchange::paper::{BpsSlippage, RateFee};
    use rust_decimal_macros::dec;
    use strategy::{Context, Strategy};

    fn paper() -> PaperConfig {
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(100000)),
        }
    }

    fn order(side: Side) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(dec!(1))),
            kind: OrderKind::Market,
        }
    }

    /// Alternates on even hours: buys on its 1st, 3rd… decision, sells on
    /// the 2nd, 4th… Its counter is in `save_state` when `saves`, so a restart
    /// carries on the alternation; without it, a restart starts over and
    /// buys again where the uninterrupted run sold.
    struct Toggle {
        n: u64,
        saves: bool,
    }
    impl Strategy for Toggle {
        fn name(&self) -> &str {
            "toggle"
        }
        fn on_candle(&mut self, _: &mut Context, c: &Candle) -> Vec<Signal> {
            // Even hours only: the order fills at the odd bar's open, so
            // every cut after an odd bar is idle.
            if (c.open_time.as_millis() / 3_600_000) % 2 == 1 {
                return vec![];
            }
            self.n += 1;
            vec![order(if self.n % 2 == 1 {
                Side::Buy
            } else {
                Side::Sell
            })]
        }
        fn save_state(&self) -> Option<serde_json::Value> {
            self.saves.then(|| serde_json::json!({ "n": self.n }))
        }
        fn restore_state(
            &mut self,
            s: serde_json::Value,
            _: &Context,
        ) -> Result<(), StrategyError> {
            self.n = s["n"].as_u64().unwrap_or(0);
            Ok(())
        }
    }

    fn bars() -> Vec<Candle> {
        let id = btcusdt().id.clone();
        (0..8)
            .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
            .collect()
    }

    fn config() -> EngineConfig {
        EngineConfig {
            whitelist: vec![btcusdt().id.clone()],
            history_len: 10,
            ..Default::default()
        }
    }

    async fn check(saves: bool) -> Result<RestartReport, RestartError> {
        check_restart(
            &[btcusdt()],
            &bars(),
            paper(),
            &move |_| Ok(Box::new(Toggle { n: 0, saves })),
            &Params::default(),
            &config(),
            7,
        )
        .await
    }

    /// Bars 0–7, cuts after bars 0–6: after an even bar its order still
    /// works (skipped), after an odd one it has filled: cuts 1, 3, 5 are
    /// compared. Holding after bar 1 (bought at bar 1's open) and bar 5;
    /// flat after bar 3. Saving its counter, the toggle survives them all.
    #[tokio::test]
    async fn a_strategy_that_saves_its_state_survives_every_cut() {
        let r = check(true).await.unwrap();
        assert_eq!(
            r,
            RestartReport {
                cuts: 3,
                while_holding: 2
            }
        );
    }

    /// Without its counter it restarts its alternation: caught.
    #[tokio::test]
    async fn a_strategy_that_forgets_its_state_is_caught() {
        let err = check(false).await.unwrap_err();
        assert!(matches!(err, RestartError::Changed { .. }), "{err}");
    }
}
