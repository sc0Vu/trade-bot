//! Binance's trading status per pair, turned into
//! `MarketEvent::TradingStatus`.
//!
//! A pair can stop trading with no schedule (`exchangeInfo` status `HALT`
//! or `BREAK`: a suspension) and resume later. [`StatusWatch`] wraps the
//! market-data feed: it asks for the whitelisted pairs' status before the
//! first event and then every `every`, and slips a status event in front of
//! the next market event for each pair whose status changed (at start:
//! each pair not trading). Like the delisting watch it stamps events with
//! the next market event's time, never the wall clock, and a failed poll is
//! a warning retried at the next poll. Every network serves `exchangeInfo`,
//! so demo and testnet runs see halts too.

use crate::{ExchangeError, MarketData};
use async_trait::async_trait;
use bot_core::{InstrumentId, MarketEvent, Symbol};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use super::PublicClient;

/// Where pair statuses come from: Binance, or a fake in tests.
#[async_trait]
pub trait StatusSource: Send {
    /// Trading (`true`) or not, per symbol asked.
    async fn statuses(
        &mut self,
        symbols: &[Symbol],
    ) -> Result<HashMap<Symbol, bool>, ExchangeError>;
}

/// `GET /api/v3/exchangeInfo?symbols=[..]` (weight 20).
pub struct BinanceStatus(pub PublicClient);

#[async_trait]
impl StatusSource for BinanceStatus {
    async fn statuses(
        &mut self,
        symbols: &[Symbol],
    ) -> Result<HashMap<Symbol, bool>, ExchangeError> {
        self.0.trading_status(symbols).await
    }
}

/// A feed that also reports halts and resumptions; see the module docs.
pub struct StatusWatch {
    inner: Box<dyn MarketData>,
    source: Box<dyn StatusSource>,
    universe: Vec<InstrumentId>,
    every: Duration,
    last_poll: Option<Instant>,
    /// Pairs the events sent so far report halted.
    halted: HashSet<InstrumentId>,
    queue: VecDeque<MarketEvent>,
}

impl StatusWatch {
    pub fn new(
        inner: Box<dyn MarketData>,
        source: Box<dyn StatusSource>,
        universe: Vec<InstrumentId>,
        every: Duration,
    ) -> Self {
        Self {
            inner,
            source,
            universe,
            every,
            last_poll: None,
            halted: HashSet::new(),
            queue: VecDeque::new(),
        }
    }
}

#[async_trait]
impl MarketData for StatusWatch {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        if let Some(e) = self.queue.pop_front() {
            return Ok(Some(e));
        }
        let Some(event) = self.inner.next().await? else {
            return Ok(None);
        };
        if self.last_poll.is_none_or(|t| t.elapsed() >= self.every) {
            self.last_poll = Some(Instant::now());
            let symbols: Vec<Symbol> = self.universe.iter().map(|i| i.symbol().clone()).collect();
            match self.source.statuses(&symbols).await {
                Ok(status) => {
                    for id in &self.universe {
                        let Some(&trading) = status.get(id.symbol()) else {
                            continue;
                        };
                        let was_halted = self.halted.contains(id);
                        if trading == was_halted {
                            if trading {
                                self.halted.remove(id);
                                tracing::info!(instrument = %id, "Binance reports trading again");
                            } else {
                                self.halted.insert(id.clone());
                                tracing::warn!(instrument = %id, "Binance reports the pair halted");
                            }
                            self.queue.push_back(MarketEvent::TradingStatus {
                                instrument: id.clone(),
                                trading,
                                ts: event.ts(),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pair status unavailable; retrying at the next poll")
                }
            }
        }
        if self.queue.is_empty() {
            return Ok(Some(event));
        }
        self.queue.push_back(event);
        Ok(self.queue.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::Timestamp;
    use std::sync::{Arc, Mutex};

    struct Feed(VecDeque<MarketEvent>);

    #[async_trait]
    impl MarketData for Feed {
        async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
            Ok(self.0.pop_front())
        }
    }

    type Answers = Arc<Mutex<VecDeque<Result<HashMap<Symbol, bool>, ExchangeError>>>>;

    struct Script(Answers);

    #[async_trait]
    impl StatusSource for Script {
        async fn statuses(&mut self, _: &[Symbol]) -> Result<HashMap<Symbol, bool>, ExchangeError> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(HashMap::new()))
        }
    }

    fn btc() -> InstrumentId {
        "binance:BTCUSDT".parse().unwrap()
    }

    fn tick(ts: i64) -> MarketEvent {
        MarketEvent::SessionOpen {
            venue: "binance".parse().unwrap(),
            ts: Timestamp::from_millis(ts),
        }
    }

    fn status(trading: bool, ts: i64) -> MarketEvent {
        MarketEvent::TradingStatus {
            instrument: btc(),
            trading,
            ts: Timestamp::from_millis(ts),
        }
    }

    fn answer(trading: bool) -> Result<HashMap<Symbol, bool>, ExchangeError> {
        Ok(HashMap::from([(Symbol::new("BTCUSDT").unwrap(), trading)]))
    }

    /// Trading at start says nothing; a halt is reported in front of the
    /// next event with its time, once; a failed poll changes nothing; the
    /// resumption is reported once.
    #[tokio::test]
    async fn reports_halts_and_resumptions_once() {
        let answers: Answers = Arc::new(Mutex::new(VecDeque::from(vec![
            answer(true),
            answer(false),
            answer(false),
            Err(ExchangeError::Http("down".into())),
            answer(true),
        ])));
        let feed = Feed((1..=5).map(|i| tick(10 * i)).collect());
        let mut w = StatusWatch::new(
            Box::new(feed),
            Box::new(Script(answers)),
            vec![btc()],
            Duration::ZERO,
        );
        let mut got = Vec::new();
        while let Some(e) = w.next().await.unwrap() {
            got.push(e);
        }
        assert_eq!(
            got,
            vec![
                tick(10),
                status(false, 20),
                tick(20),
                tick(30),
                tick(40),
                status(true, 50),
                tick(50),
            ]
        );
    }
}
