//! Binance's spot delisting schedule, turned into `MarketEvent::Delisting`.
//!
//! `GET /sapi/v1/spot/delist-schedule` (API key, no signature, weight 100)
//! lists `[{"delistTime": ms, "symbols": [..]}]`: the coins Binance has
//! announced it will stop trading. Only the live host serves it (demo and
//! testnet answer 404), so only a live run watches it. [`DelistWatch`]
//! wraps the market-data feed: it polls the schedule before the first
//! event and then every `every`, and slips a delisting event in front of
//! the next market event for each whitelisted coin that is newly
//! scheduled, rescheduled, or (before its time) taken off the list. The
//! event carries the next market event's time, never the wall clock, so
//! the stream's time never goes backwards (a reconnect's backfill bars are
//! older than the wall clock). A failed poll is a warning, retried at the
//! next poll: the engine's exit acts on what is known, and an unreachable
//! schedule must not stop trading.

use crate::{ExchangeError, MarketData};
use async_trait::async_trait;
use bot_core::{InstrumentId, MarketEvent, Symbol, Timestamp, VenueId};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::{Method, SignedClient};

/// One poll's answer: symbol → delisting time.
pub type Schedule = HashMap<Symbol, Timestamp>;

/// Where the schedule comes from: Binance, or a fake in tests.
#[async_trait]
pub trait ScheduleSource: Send {
    async fn schedule(&mut self) -> Result<Schedule, ExchangeError>;
}

/// The live endpoint.
pub struct BinanceSchedule(pub SignedClient);

#[async_trait]
impl ScheduleSource for BinanceSchedule {
    async fn schedule(&mut self) -> Result<Schedule, ExchangeError> {
        let v = self
            .0
            .keyed(Method::Get, "/sapi/v1/spot/delist-schedule", &[])
            .await?;
        parse(&v)
    }
}

/// Reads `[{"delistTime": ms, "symbols": ["ADAUSDT"]}]`. A malformed
/// answer is an error, not an empty schedule.
pub fn parse(v: &Value) -> Result<Schedule, ExchangeError> {
    let bad = |what: &str| ExchangeError::Decode(format!("delist-schedule: {what}"));
    let mut out = Schedule::new();
    for entry in v.as_array().ok_or_else(|| bad("not an array"))? {
        let at = entry["delistTime"]
            .as_i64()
            .ok_or_else(|| bad("delistTime"))?;
        for s in entry["symbols"].as_array().ok_or_else(|| bad("symbols"))? {
            let s = s.as_str().ok_or_else(|| bad("symbol"))?;
            let symbol = Symbol::new(s).map_err(|e| bad(&e.to_string()))?;
            out.insert(symbol, Timestamp::from_millis(at));
        }
    }
    Ok(out)
}

/// A feed that also reports the venue's delisting schedule; see the
/// module docs.
pub struct DelistWatch {
    inner: Box<dyn MarketData>,
    source: Box<dyn ScheduleSource>,
    venue: VenueId,
    universe: Vec<InstrumentId>,
    every: Duration,
    last_poll: Option<Instant>,
    /// What the events sent so far announced.
    known: HashMap<InstrumentId, Timestamp>,
    queue: VecDeque<MarketEvent>,
}

impl DelistWatch {
    pub fn new(
        inner: Box<dyn MarketData>,
        source: Box<dyn ScheduleSource>,
        venue: VenueId,
        universe: Vec<InstrumentId>,
        every: Duration,
    ) -> Self {
        Self {
            inner,
            source,
            venue,
            universe,
            every,
            last_poll: None,
            known: HashMap::new(),
            queue: VecDeque::new(),
        }
    }

    fn due(&self) -> bool {
        self.last_poll.is_none_or(|t| t.elapsed() >= self.every)
    }

    /// Events for what changed since the last poll, stamped `ts`.
    fn changes(&mut self, schedule: &Schedule, ts: Timestamp) -> Vec<MarketEvent> {
        let mut out = Vec::new();
        for id in &self.universe {
            let now = (id.venue() == &self.venue)
                .then(|| schedule.get(id.symbol()).copied())
                .flatten();
            let before = self.known.get(id).copied();
            match (before, now) {
                (b, Some(at)) if b != Some(at) => {
                    tracing::warn!(instrument = %id, at = %at, "Binance scheduled a delisting");
                    self.known.insert(id.clone(), at);
                    out.push(MarketEvent::Delisting {
                        instrument: id.clone(),
                        at: Some(at),
                        ts,
                    });
                }
                // Off the list before its time: withdrawn. After it, the
                // coin simply delisted and left the list.
                (Some(at), None) if ts < at => {
                    tracing::info!(instrument = %id, "Binance withdrew a delisting");
                    self.known.remove(id);
                    out.push(MarketEvent::Delisting {
                        instrument: id.clone(),
                        at: None,
                        ts,
                    });
                }
                _ => {}
            }
        }
        out
    }
}

#[async_trait]
impl MarketData for DelistWatch {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        if let Some(e) = self.queue.pop_front() {
            return Ok(Some(e));
        }
        let Some(event) = self.inner.next().await? else {
            return Ok(None);
        };
        if self.due() {
            self.last_poll = Some(Instant::now());
            match self.source.schedule().await {
                Ok(schedule) => {
                    let changes = self.changes(&schedule, event.ts());
                    self.queue.extend(changes);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "delist schedule unavailable; retrying at the next poll")
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
    use std::sync::{Arc, Mutex};

    struct Feed(VecDeque<MarketEvent>);

    #[async_trait]
    impl MarketData for Feed {
        async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
            Ok(self.0.pop_front())
        }
    }

    /// Hands out the next scripted answer per poll.
    #[derive(Clone)]
    struct Script(Arc<Mutex<VecDeque<Result<Schedule, ExchangeError>>>>);

    #[async_trait]
    impl ScheduleSource for Script {
        async fn schedule(&mut self) -> Result<Schedule, ExchangeError> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Schedule::new()))
        }
    }

    fn btc() -> InstrumentId {
        "binance:BTCUSDT".parse().unwrap()
    }

    /// A non-candle stand-in for a market event at `ts`.
    fn tick(ts: i64) -> MarketEvent {
        MarketEvent::SessionOpen {
            venue: "binance".parse().unwrap(),
            ts: Timestamp::from_millis(ts),
        }
    }

    fn sched(pairs: &[(&str, i64)]) -> Schedule {
        pairs
            .iter()
            .map(|(s, t)| (Symbol::new(s).unwrap(), Timestamp::from_millis(*t)))
            .collect()
    }

    async fn drain(w: &mut DelistWatch) -> Vec<MarketEvent> {
        let mut out = Vec::new();
        while let Some(e) = w.next().await.unwrap() {
            out.push(e);
        }
        out
    }

    fn delisting(at: Option<i64>, ts: i64) -> MarketEvent {
        MarketEvent::Delisting {
            instrument: btc(),
            at: at.map(Timestamp::from_millis),
            ts: Timestamp::from_millis(ts),
        }
    }

    #[test]
    fn parses_the_documented_shape() {
        let v: Value = serde_json::from_str(
            r#"[{"delistTime":1686161202000,"symbols":["ADAUSDT","ADABTC"]}]"#,
        )
        .unwrap();
        let s = parse(&v).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[&Symbol::new("ADABTC").unwrap()],
            Timestamp::from_millis(1_686_161_202_000)
        );
        for bad in [
            r#"{}"#,
            r#"[{"symbols":["X"]}]"#,
            r#"[{"delistTime":1,"symbols":"X"}]"#,
        ] {
            assert!(parse(&serde_json::from_str(bad).unwrap()).is_err(), "{bad}");
        }
    }

    /// Polling every event: BTC scheduled (at 100) is announced in front of
    /// the event at 10, with its time; the same answer again says nothing;
    /// a moved time is announced again; dropped before its time,
    /// withdrawn. Coins outside the whitelist (ETH) are ignored. A failed
    /// poll changes nothing.
    #[tokio::test]
    async fn announces_what_changed_in_front_of_the_next_event() {
        let answers = Script(Arc::new(Mutex::new(VecDeque::from(vec![
            Ok(sched(&[("BTCUSDT", 100), ("ETHUSDT", 50)])),
            Ok(sched(&[("BTCUSDT", 100)])),
            Err(ExchangeError::Http("down".into())),
            Ok(sched(&[("BTCUSDT", 120)])),
            Ok(sched(&[])),
        ]))));
        let feed = Feed((0..5).map(|i| tick(10 * (i + 1))).collect());
        let mut w = DelistWatch::new(
            Box::new(feed),
            Box::new(answers),
            "binance".parse().unwrap(),
            vec![btc()],
            Duration::ZERO,
        );
        assert_eq!(
            drain(&mut w).await,
            vec![
                delisting(Some(100), 10),
                tick(10),
                tick(20),
                tick(30),
                delisting(Some(120), 40),
                tick(40),
                delisting(None, 50),
                tick(50),
            ]
        );
    }

    /// Off the list after its time: it delisted, nothing to withdraw.
    #[tokio::test]
    async fn leaving_the_list_after_the_time_is_no_withdrawal() {
        let answers = Script(Arc::new(Mutex::new(VecDeque::from(vec![
            Ok(sched(&[("BTCUSDT", 15)])),
            Ok(sched(&[])),
        ]))));
        let feed = Feed(VecDeque::from(vec![tick(10), tick(20)]));
        let mut w = DelistWatch::new(
            Box::new(feed),
            Box::new(answers),
            "binance".parse().unwrap(),
            vec![btc()],
            Duration::ZERO,
        );
        assert_eq!(
            drain(&mut w).await,
            vec![delisting(Some(15), 10), tick(10), tick(20)]
        );
    }
}
