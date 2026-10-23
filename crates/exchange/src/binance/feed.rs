//! A live kline feed that survives disconnects and warms up history.
//!
//! [`ReconnectingFeed`] wraps the WebSocket stream:
//!
//! - **On first connect** it fetches the last `warmup` closed bars per coin
//!   over REST and emits them as [`MarketEvent::Backfill`], so a restarted
//!   bot's indicators start full instead of waiting out a warm-up. Those
//!   bars were decided on (or not) by the previous run; none is a decision
//!   now.
//! - **On a feed error** (dropped socket, server close, stall) it
//!   reconnects with exponential backoff (1 s, 2 s, 4 s … capped at 60 s)
//!   for at most `max_outage`; past that the error stops the run and the
//!   exit close-out runs as usual. After reconnecting it fetches the bars
//!   it missed: all but the newest per coin come as `Backfill` (history
//!   only; deciding on them would act on stale prices), the newest as
//!   `Candle`, a decision on the freshest bar.
//! - **It subscribes before it backfills**, so no bar can close unseen
//!   between the end of the backfill and the start of the stream, and it
//!   drops any bar at or before the last one it emitted for that coin: the
//!   overlap is expected.
//! - **The backfill stops at the oldest coin's newest bar.** Coins are
//!   fetched one by one; at short intervals a bar closes in between, and a
//!   later coin's extra bar queued ahead of the stream would make time run
//!   backwards. The stream delivers those bars instead. A warm-up fetches
//!   a few bars extra per coin so each still gets its full `warmup`.
//!
//! Missed bars are merged across coins by (close time, instrument), the
//! order the backtest feed uses. Errors other than feed errors (a
//! malformed message) are not retried: they say something is wrong, not
//! that the network blinked.

use super::{KlineStream, PublicClient};
use crate::{ExchangeError, MarketData};
use async_trait::async_trait;
use bot_core::{Candle, InstrumentId, Interval, MarketEvent, Timestamp};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Most bars one gap backfill fetches per coin (one Binance klines page).
const MAX_GAP: usize = 1000;
/// Extra bars per coin a warm-up fetches, so a coin still has `warmup`
/// bars once those past the cutoff are left to the stream: room for that
/// many bars to close while the coins are fetched.
const WARMUP_SLACK: usize = 60;

/// Opens a live stream of closed candles.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self) -> Result<Box<dyn MarketData>, ExchangeError>;
}

/// Serves recent closed candles over REST.
#[async_trait]
pub trait KlineSource: Send + Sync {
    /// Closed candles of `id` with close time after `since` (all recent
    /// ones if `None`), oldest first, at most the newest `max`.
    async fn recent(
        &self,
        id: &InstrumentId,
        since: Option<Timestamp>,
        max: usize,
    ) -> Result<Vec<Candle>, ExchangeError>;
}

/// See the module docs.
pub struct ReconnectingFeed {
    connector: Box<dyn Connector>,
    source: Box<dyn KlineSource>,
    ids: Vec<InstrumentId>,
    warmup: usize,
    max_outage: Duration,
    live: Option<Box<dyn MarketData>>,
    /// Close time of the last candle emitted per coin.
    last: HashMap<InstrumentId, Timestamp>,
    queue: VecDeque<MarketEvent>,
}

impl ReconnectingFeed {
    /// `warmup` bars per coin of history on first connect (the engine's
    /// `history_len`); reconnect attempts for at most `max_outage`.
    pub fn new(
        connector: Box<dyn Connector>,
        source: Box<dyn KlineSource>,
        ids: Vec<InstrumentId>,
        warmup: usize,
        max_outage: Duration,
    ) -> Self {
        Self {
            connector,
            source,
            ids,
            warmup,
            max_outage,
            live: None,
            last: HashMap::new(),
            queue: VecDeque::new(),
        }
    }

    /// Connects, retrying with backoff until `max_outage` has passed.
    async fn connect_with_retry(&mut self) -> Result<(), ExchangeError> {
        let first = self.last.is_empty();
        let deadline = tokio::time::Instant::now() + self.max_outage;
        let mut delay = FIRST_BACKOFF;
        loop {
            match self.try_connect(first).await {
                Ok(()) => return Ok(()),
                Err(e) if retryable(&e) && tokio::time::Instant::now() + delay < deadline => {
                    tracing::warn!(error = %e, retry_in_secs = delay.as_secs(), "feed connect failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_BACKOFF);
                }
                Err(e) if retryable(&e) => {
                    return Err(ExchangeError::Feed(format!(
                        "gave up reconnecting after {}s: {e}",
                        self.max_outage.as_secs()
                    )));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Subscribes, then queues what the subscription can't deliver: the
    /// warm-up on first connect, the gap since the last bar otherwise.
    async fn try_connect(&mut self, first: bool) -> Result<(), ExchangeError> {
        let live = self.connector.connect().await?;
        let mut missed: Vec<Candle> = Vec::new();
        // Newest bar each coin is known to have closed by, fetched or
        // already emitted; the oldest of them bounds the backfill.
        let mut cutoff: Option<Timestamp> = None;
        for id in &self.ids {
            let since = self.last.get(id).copied();
            if !first && since.is_none() {
                continue; // no bar of this coin yet: nothing to fill
            }
            if first && self.warmup == 0 {
                continue;
            }
            let max = if first {
                self.warmup + WARMUP_SLACK
            } else {
                MAX_GAP
            };
            let bars = self.source.recent(id, since, max).await?;
            if let Some(newest) = bars.last().map(|c| c.close_time).or(since) {
                cutoff = Some(cutoff.map_or(newest, |c| c.min(newest)));
            }
            missed.extend(bars);
        }
        // Coins are fetched one after another, so a later fetch can hold a
        // bar that closed after an earlier one returned. Queued, it would
        // precede the earlier coin's copy of that bar arriving on the
        // stream: time going backwards. Leave everything past the cutoff
        // to the stream, which has it: it was subscribed before any fetch.
        missed.retain(|c| {
            cutoff.is_none_or(|t| c.close_time <= t)
                && self
                    .last
                    .get(&c.instrument)
                    .is_none_or(|t| c.close_time > *t)
        });
        if first {
            // Each coin's bars are still together, oldest first: keep the
            // newest `warmup` of each.
            let mut kept: HashMap<InstrumentId, usize> = HashMap::new();
            let mut keep: Vec<bool> = missed
                .iter()
                .rev()
                .map(|c| {
                    let n = kept.entry(c.instrument.clone()).or_default();
                    *n += 1;
                    *n <= self.warmup
                })
                .collect();
            keep.reverse();
            let mut keep = keep.into_iter();
            missed.retain(|_| keep.next().unwrap_or(false));
        }
        missed.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
        let newest: HashMap<&InstrumentId, Timestamp> = missed
            .iter()
            .map(|c| (&c.instrument, c.close_time))
            .collect();
        let decide: Vec<bool> = missed
            .iter()
            .map(|c| !first && newest.get(&c.instrument) == Some(&c.close_time))
            .collect();
        let backfilled = decide.iter().filter(|d| !**d).count();
        metrics::counter!(crate::metrics::FEED_BACKFILLED).increment(backfilled as u64);
        for (c, decide) in missed.into_iter().zip(decide) {
            self.last.insert(c.instrument.clone(), c.close_time);
            self.queue.push_back(if decide {
                MarketEvent::Candle(c)
            } else {
                MarketEvent::Backfill(c)
            });
        }
        self.live = Some(live);
        Ok(())
    }
}

/// Network trouble worth retrying, as opposed to bad data.
fn retryable(e: &ExchangeError) -> bool {
    matches!(e, ExchangeError::Feed(_) | ExchangeError::Http(_))
}

#[async_trait]
impl MarketData for ReconnectingFeed {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        loop {
            if self.live.is_none() {
                self.connect_with_retry().await?;
            }
            if let Some(e) = self.queue.pop_front() {
                return Ok(Some(e));
            }
            let Some(live) = self.live.as_mut() else {
                continue;
            };
            let got = live.next().await;
            match got {
                Ok(Some(MarketEvent::Candle(c))) => {
                    if self
                        .last
                        .get(&c.instrument)
                        .is_some_and(|t| c.close_time <= *t)
                    {
                        continue; // already delivered by the backfill
                    }
                    self.last.insert(c.instrument.clone(), c.close_time);
                    return Ok(Some(MarketEvent::Candle(c)));
                }
                Ok(Some(other)) => return Ok(Some(other)),
                // A live stream never ends on its own: treat it as a drop.
                Ok(None) => self.drop_live("stream ended"),
                Err(ExchangeError::Feed(msg)) => self.drop_live(&msg),
                Err(e) => return Err(e),
            }
        }
    }
}

impl ReconnectingFeed {
    fn drop_live(&mut self, why: &str) {
        tracing::warn!(reason = why, "market data feed lost; reconnecting");
        metrics::counter!(crate::metrics::FEED_RECONNECTS).increment(1);
        self.live = None;
    }
}

/// [`Connector`] for Binance's kline WebSocket.
pub struct WsConnector {
    pub url: String,
    pub ids: Vec<InstrumentId>,
    pub interval: Interval,
}

#[async_trait]
impl Connector for WsConnector {
    async fn connect(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(
            KlineStream::connect(&self.url, &self.ids, self.interval).await?,
        ))
    }
}

/// [`KlineSource`] over Binance's REST klines (through the provider's
/// shared rate limiter).
pub struct RestKlines {
    pub client: PublicClient,
    pub interval: Interval,
}

#[async_trait]
impl KlineSource for RestKlines {
    async fn recent(
        &self,
        id: &InstrumentId,
        since: Option<Timestamp>,
        max: usize,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let bar = self.interval.as_millis();
        let start = match since {
            Some(t) => t.as_millis() + 1,
            None => now - (max as i64 + 1) * bar,
        };
        let mut bars = self
            .client
            .klines(
                id.symbol(),
                self.interval,
                Timestamp::from_millis(start),
                Timestamp::from_millis(now),
            )
            .await?;
        bars.retain(|c| since.is_none_or(|s| c.close_time > s));
        let drop = bars.len().saturating_sub(max);
        bars.drain(..drop);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{CandleSpec, Notional, Price, Qty};
    use std::sync::{Arc, Mutex};

    const MIN: i64 = 60_000;

    fn id(s: &str) -> InstrumentId {
        format!("binance:{s}").parse().unwrap()
    }

    /// Bar `n` of `coin` (1m, flat at 100 + n).
    fn bar(coin: &str, n: i64) -> Candle {
        let p = Price::new(bot_core::Decimal::from(100 + n));
        Candle::new(CandleSpec {
            instrument: id(coin),
            interval: Interval::M1,
            open_time: Timestamp::from_millis(n * MIN),
            close_time: Timestamp::from_millis(n * MIN + MIN - 1),
            open: p,
            high: p,
            low: p,
            close: p,
            volume: Qty::new(bot_core::Decimal::ONE),
            quote_volume: Notional::new(p.get()),
        })
        .unwrap()
    }

    /// A scripted live stream: these events, then this error.
    struct Script(VecDeque<MarketEvent>, Option<ExchangeError>);

    #[async_trait]
    impl MarketData for Script {
        async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
            match self.0.pop_front() {
                Some(e) => Ok(Some(e)),
                None => match self.1.take() {
                    Some(e) => Err(e),
                    // Then silence, like a quiet socket.
                    None => std::future::pending().await,
                },
            }
        }
    }

    /// Hands out scripted connections in order; `Err` entries fail the
    /// connect itself.
    #[derive(Clone, Default)]
    struct Connections(
        Arc<Mutex<VecDeque<Result<Script, ExchangeError>>>>,
        Arc<Mutex<u32>>,
    );

    impl Connections {
        fn push(&self, c: Result<Script, ExchangeError>) {
            self.0.lock().unwrap().push_back(c);
        }
        fn attempts(&self) -> u32 {
            *self.1.lock().unwrap()
        }
    }

    #[async_trait]
    impl Connector for Connections {
        async fn connect(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
            *self.1.lock().unwrap() += 1;
            match self.0.lock().unwrap().pop_front() {
                Some(Ok(s)) => Ok(Box::new(s)),
                Some(Err(e)) => Err(e),
                None => Err(ExchangeError::Feed("no more connections".into())),
            }
        }
    }

    /// REST history: every bar of each coin, filtered like Binance's.
    struct History(HashMap<InstrumentId, Vec<Candle>>);

    #[async_trait]
    impl KlineSource for History {
        async fn recent(
            &self,
            id: &InstrumentId,
            since: Option<Timestamp>,
            max: usize,
        ) -> Result<Vec<Candle>, ExchangeError> {
            let mut v: Vec<Candle> = self.0[id]
                .iter()
                .filter(|c| since.is_none_or(|s| c.close_time > s))
                .cloned()
                .collect();
            let drop = v.len().saturating_sub(max);
            v.drain(..drop);
            Ok(v)
        }
    }

    fn live(events: Vec<Candle>, then: Option<ExchangeError>) -> Script {
        Script(events.into_iter().map(MarketEvent::Candle).collect(), then)
    }

    fn drop_err() -> Option<ExchangeError> {
        Some(ExchangeError::Feed("websocket closed".into()))
    }

    fn feed(
        conns: &Connections,
        history: Vec<(&str, Vec<i64>)>,
        ids: &[&str],
        warmup: usize,
    ) -> ReconnectingFeed {
        let h = History(
            history
                .into_iter()
                .map(|(c, ns)| (id(c), ns.into_iter().map(|n| bar(c, n)).collect()))
                .collect(),
        );
        ReconnectingFeed::new(
            Box::new(conns.clone()),
            Box::new(h),
            ids.iter().map(|s| id(s)).collect(),
            warmup,
            Duration::from_secs(600),
        )
    }

    /// Short descriptions of the next `n` events: "B:BTCUSDT@3" for a
    /// backfilled bar 3, "C:BTCUSDT@3" for a live one.
    async fn take(f: &mut ReconnectingFeed, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            let e = f.next().await.unwrap().unwrap();
            let (k, c) = match &e {
                MarketEvent::Candle(c) => ("C", c),
                MarketEvent::Backfill(c) => ("B", c),
                _ => panic!("{e:?}"),
            };
            out.push(format!(
                "{k}:{}@{}",
                c.instrument.symbol(),
                c.open_time.as_millis() / MIN
            ));
        }
        out
    }

    /// First connect: the last `warmup` closed bars are history-only, then
    /// the live stream follows.
    #[tokio::test(start_paused = true)]
    async fn first_connect_warms_up_history() {
        let conns = Connections::default();
        conns.push(Ok(live(vec![bar("BTCUSDT", 5)], None)));
        let mut f = feed(
            &conns,
            vec![("BTCUSDT", vec![0, 1, 2, 3, 4])],
            &["BTCUSDT"],
            3,
        );
        assert_eq!(
            take(&mut f, 4).await,
            vec!["B:BTCUSDT@2", "B:BTCUSDT@3", "B:BTCUSDT@4", "C:BTCUSDT@5"]
        );
    }

    /// Live 5, 6, then the socket drops. Bars 7 and 8 close while it's
    /// down. After reconnecting: 7 is history only, 8 (the newest) is a
    /// decision, and the stream's own copy of 8 is dropped before 9.
    #[tokio::test(start_paused = true)]
    async fn reconnect_backfills_the_gap_newest_as_decision() {
        let conns = Connections::default();
        conns.push(Ok(live(
            vec![bar("BTCUSDT", 5), bar("BTCUSDT", 6)],
            drop_err(),
        )));
        conns.push(Ok(live(vec![bar("BTCUSDT", 8), bar("BTCUSDT", 9)], None)));
        let mut f = feed(
            &conns,
            vec![("BTCUSDT", (0..=8).collect())],
            &["BTCUSDT"],
            0,
        );
        assert_eq!(
            take(&mut f, 5).await,
            vec![
                "C:BTCUSDT@5",
                "C:BTCUSDT@6",
                "B:BTCUSDT@7",
                "C:BTCUSDT@8",
                "C:BTCUSDT@9"
            ]
        );
        assert_eq!(conns.attempts(), 2);
    }

    /// Two coins' missed bars interleave by close time, then instrument, as
    /// the backtest feed orders them.
    #[tokio::test(start_paused = true)]
    async fn gap_from_several_coins_is_merged_in_order() {
        let conns = Connections::default();
        conns.push(Ok(live(
            vec![bar("BTCUSDT", 1), bar("ETHUSDT", 1)],
            drop_err(),
        )));
        conns.push(Ok(live(vec![], None)));
        let mut f = feed(
            &conns,
            vec![("BTCUSDT", vec![0, 1, 2, 3]), ("ETHUSDT", vec![0, 1, 2, 3])],
            &["BTCUSDT", "ETHUSDT"],
            0,
        );
        assert_eq!(
            take(&mut f, 6).await,
            vec![
                "C:BTCUSDT@1",
                "C:ETHUSDT@1",
                "B:BTCUSDT@2",
                "B:ETHUSDT@2",
                "C:BTCUSDT@3",
                "C:ETHUSDT@3"
            ]
        );
    }

    /// A bar closes between the warm-up fetches: BTC's history ends at 3,
    /// ETH's (fetched later) at 5. Queuing ETH 4 and 5 would put them
    /// before the stream's BTC 4, time going backwards; they come from the
    /// stream instead, in order.
    #[tokio::test(start_paused = true)]
    async fn warmup_stops_at_the_oldest_coins_newest_bar() {
        let conns = Connections::default();
        conns.push(Ok(live(
            vec![
                bar("BTCUSDT", 4),
                bar("ETHUSDT", 4),
                bar("BTCUSDT", 5),
                bar("ETHUSDT", 5),
            ],
            None,
        )));
        let mut f = feed(
            &conns,
            vec![
                ("BTCUSDT", (0..=3).collect()),
                ("ETHUSDT", (0..=5).collect()),
            ],
            &["BTCUSDT", "ETHUSDT"],
            2,
        );
        assert_eq!(
            take(&mut f, 8).await,
            vec![
                "B:BTCUSDT@2",
                "B:ETHUSDT@2",
                "B:BTCUSDT@3",
                "B:ETHUSDT@3",
                "C:BTCUSDT@4",
                "C:ETHUSDT@4",
                "C:BTCUSDT@5",
                "C:ETHUSDT@5"
            ]
        );
    }

    /// Gap backfill, same race: BTC has nothing new past its last bar 1
    /// yet, ETH already has 2 and 3. Those come from the stream after
    /// BTC 2, not ahead of it.
    #[tokio::test(start_paused = true)]
    async fn gap_stops_at_a_coin_with_nothing_new() {
        let conns = Connections::default();
        conns.push(Ok(live(
            vec![bar("BTCUSDT", 1), bar("ETHUSDT", 1)],
            drop_err(),
        )));
        conns.push(Ok(live(
            vec![
                bar("BTCUSDT", 2),
                bar("ETHUSDT", 2),
                bar("BTCUSDT", 3),
                bar("ETHUSDT", 3),
            ],
            None,
        )));
        let mut f = feed(
            &conns,
            vec![("BTCUSDT", vec![0, 1]), ("ETHUSDT", vec![0, 1, 2, 3])],
            &["BTCUSDT", "ETHUSDT"],
            0,
        );
        assert_eq!(
            take(&mut f, 6).await,
            vec![
                "C:BTCUSDT@1",
                "C:ETHUSDT@1",
                "C:BTCUSDT@2",
                "C:ETHUSDT@2",
                "C:BTCUSDT@3",
                "C:ETHUSDT@3"
            ]
        );
    }

    /// Two failed connects: retried after 1 s, then 2 s.
    #[tokio::test(start_paused = true)]
    async fn failed_connects_back_off() {
        let conns = Connections::default();
        conns.push(Ok(live(vec![bar("BTCUSDT", 0)], drop_err())));
        conns.push(Err(ExchangeError::Feed("refused".into())));
        conns.push(Err(ExchangeError::Http("timeout".into())));
        conns.push(Ok(live(vec![bar("BTCUSDT", 1)], None)));
        let mut f = feed(&conns, vec![("BTCUSDT", vec![0])], &["BTCUSDT"], 0);
        take(&mut f, 1).await;
        let start = tokio::time::Instant::now();
        assert_eq!(take(&mut f, 1).await, vec!["C:BTCUSDT@1"]);
        assert_eq!(start.elapsed(), Duration::from_secs(3));
        assert_eq!(conns.attempts(), 4);
    }

    /// Down for longer than max_outage: the error stops the run.
    #[tokio::test(start_paused = true)]
    async fn gives_up_after_the_outage_limit() {
        let conns = Connections::default();
        conns.push(Ok(live(vec![bar("BTCUSDT", 0)], drop_err())));
        let mut f = feed(&conns, vec![("BTCUSDT", vec![0])], &["BTCUSDT"], 0);
        take(&mut f, 1).await;
        let start = tokio::time::Instant::now();
        let err = f.next().await.unwrap_err();
        assert!(
            err.to_string().contains("gave up reconnecting after 600s"),
            "{err}"
        );
        assert!(start.elapsed() <= Duration::from_secs(600));
    }

    /// Bad data is not a network blip: no reconnect.
    #[tokio::test(start_paused = true)]
    async fn bad_data_is_not_retried() {
        let conns = Connections::default();
        conns.push(Ok(live(
            vec![],
            Some(ExchangeError::Decode("garbled".into())),
        )));
        let mut f = feed(&conns, vec![("BTCUSDT", vec![])], &["BTCUSDT"], 0);
        assert!(matches!(f.next().await, Err(ExchangeError::Decode(_))));
        assert_eq!(conns.attempts(), 1);
    }
}
