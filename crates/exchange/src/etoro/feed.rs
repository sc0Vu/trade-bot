//! eToro's market data as a feed, and [`EtoroProvider`].
//!
//! eToro has no public kline stream we can use without parsing its
//! WebSocket's undocumented interval choice, so the feed polls: once at
//! start, the last `warmup` closed bars per instrument as `Backfill`; then
//! `lag` after every interval boundary, the newest closed bars, each new
//! one as `Candle`, in (close time, id) order. A closed market just
//! produces no bar (a stock over the weekend): not a stall. A bar that
//! shows up after a newer one was already delivered can't be delivered
//! without time running backwards: dropped with a warning. A poll that
//! fails is retried each boundary; failing for longer than `max_outage`
//! stops the run, as Binance's feed does.

use super::VENUE;
use super::broker::{EtoroApi, EtoroBroker, reconcile};
use super::client::EtoroClient;
use crate::{Books, Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{AssetClass, Candle, Instrument, InstrumentId, Interval, MarketEvent, Timestamp};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// After a boundary, how long eToro gets to publish the closed bar.
pub const LAG: Duration = Duration::from_secs(5);
/// Bars asked for per poll (a missed boundary is caught up).
const POLL_BARS: usize = 5;

/// See the module docs.
pub struct EtoroFeed {
    client: EtoroClient,
    ids: Vec<(InstrumentId, i64)>,
    interval: Interval,
    warmup: usize,
    max_outage: Duration,
    lag: Duration,
    last: HashMap<InstrumentId, Timestamp>,
    newest: Option<Timestamp>,
    queue: VecDeque<MarketEvent>,
    warmed: bool,
    failing_since: Option<Instant>,
}

impl EtoroFeed {
    pub fn new(
        client: EtoroClient,
        ids: Vec<(InstrumentId, i64)>,
        interval: Interval,
        warmup: usize,
        max_outage: Duration,
    ) -> Self {
        Self {
            client,
            ids,
            interval,
            warmup,
            max_outage,
            lag: LAG,
            last: HashMap::new(),
            newest: None,
            queue: VecDeque::new(),
            warmed: false,
            failing_since: None,
        }
    }

    /// Queues `bars` (any instruments) that are new, in order; `backfill`
    /// marks history.
    fn take(&mut self, mut bars: Vec<Candle>, backfill: bool) {
        bars.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
        for c in bars {
            if self
                .last
                .get(&c.instrument)
                .is_some_and(|t| c.close_time <= *t)
            {
                continue;
            }
            if self.newest.is_some_and(|n| c.close_time < n) {
                tracing::warn!(instrument = %c.instrument, "eToro bar arrived after newer ones; dropped");
                self.last.insert(c.instrument.clone(), c.close_time);
                continue;
            }
            self.last.insert(c.instrument.clone(), c.close_time);
            self.newest = Some(c.close_time);
            self.queue.push_back(if backfill {
                MarketEvent::Backfill(c)
            } else {
                MarketEvent::Candle(c)
            });
        }
    }

    async fn poll(&self, n: usize) -> Result<Vec<Candle>, ExchangeError> {
        let mut all = Vec::new();
        for (id, etoro_id) in &self.ids {
            all.extend(self.client.recent(*etoro_id, id, self.interval, n).await?);
        }
        Ok(all)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[async_trait]
impl MarketData for EtoroFeed {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        loop {
            if let Some(e) = self.queue.pop_front() {
                return Ok(Some(e));
            }
            if !self.warmed {
                let bars = self.poll(self.warmup).await?;
                self.take(bars, true);
                self.warmed = true;
                continue;
            }
            let bar = self.interval.as_millis();
            let now = now_ms();
            let next = (now.div_euclid(bar) + 1) * bar;
            let wait = u64::try_from(next - now).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(wait) + self.lag).await;
            match self.poll(POLL_BARS).await {
                Ok(bars) => {
                    self.failing_since = None;
                    self.take(bars, false);
                }
                Err(e) => {
                    let since = *self.failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > self.max_outage {
                        return Err(ExchangeError::Feed(format!(
                            "eToro market data unavailable for over {:?}: {e}",
                            self.max_outage
                        )));
                    }
                    tracing::warn!(error = %e, "eToro poll failed; retrying at the next bar");
                }
            }
        }
    }
}

/// How an eToro run connects.
#[derive(Clone, Debug)]
pub struct EtoroConfig {
    pub interval: Interval,
    pub warmup: usize,
    pub max_outage: Duration,
}

/// eToro as a provider: [`EtoroFeed`] and [`EtoroBroker`] on one account.
pub struct EtoroProvider {
    client: EtoroClient,
    config: EtoroConfig,
    universe: Vec<InstrumentId>,
    books: Arc<dyn Books>,
    run_tag: String,
}

impl EtoroProvider {
    pub fn new(
        client: EtoroClient,
        config: EtoroConfig,
        universe: Vec<InstrumentId>,
        books: Arc<dyn Books>,
    ) -> Self {
        Self {
            client,
            config,
            universe,
            books,
            run_tag: crate::binance::run_tag(now_ms()),
        }
    }

    pub fn run_tag(&self) -> &str {
        &self.run_tag
    }

    async fn resolved(&self) -> Result<Vec<(Instrument, i64)>, ExchangeError> {
        let mut out = Vec::with_capacity(self.universe.len());
        for id in &self.universe {
            if id.venue().as_str() != VENUE {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            let e = self.client.instrument(id.symbol()).await?;
            out.push((e.instrument, e.etoro_id));
        }
        Ok(out)
    }

    /// Settles every order still in flight and records its fills; positions
    /// stay. Call it however a run ends.
    pub async fn close_out(&self) -> Result<usize, ExchangeError> {
        let instruments = self.resolved().await?;
        let api: &dyn EtoroApi = &self.client;
        Ok(
            reconcile(api, &instruments, &*self.books, Duration::from_secs(1))
                .await?
                .len(),
        )
    }
}

#[async_trait]
impl Provider for EtoroProvider {
    fn id(&self) -> &str {
        "etoro"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        let mut ids = Vec::with_capacity(self.universe.len());
        for id in &self.universe {
            let (etoro_id, _, _) = self.client.lookup(id.symbol()).await?;
            ids.push((id.clone(), etoro_id));
        }
        Ok(Box::new(EtoroFeed::new(
            self.client.clone(),
            ids,
            self.config.interval,
            self.config.warmup,
            self.config.max_outage,
        )))
    }

    /// Reconciles the books with the account, then opens the broker.
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let instruments = self.resolved().await?;
        let api: &dyn EtoroApi = &self.client;
        let recovered = reconcile(api, &instruments, &*self.books, Duration::from_secs(1)).await?;
        tracing::info!(fills_recovered = recovered.len(), "reconciled with eToro");
        Ok(Box::new(
            EtoroBroker::open(
                Box::new(self.client.clone()),
                instruments,
                self.run_tag.clone(),
                self.books.clone(),
            )
            .await?
            .with_recovered(recovered),
        ))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.venue().as_str() != VENUE {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            out.push(self.client.instrument(id.symbol()).await?.instrument);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etoro::{Account, EtoroKeys};
    use bot_core::{CandleSpec, Notional, Price, Qty};
    use rust_decimal_macros::dec;

    fn bar(sym: &str, i: i64) -> Candle {
        let p = Price::new(dec!(100));
        Candle::new(CandleSpec {
            instrument: format!("etoro:{sym}").parse().unwrap(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(i * 3_600_000),
            close_time: Timestamp::from_millis(i * 3_600_000 + 3_599_999),
            open: p,
            high: p,
            low: p,
            close: p,
            volume: Qty::new(dec!(0)),
            quote_volume: Notional::ZERO,
        })
        .unwrap()
    }

    fn feed() -> EtoroFeed {
        EtoroFeed::new(
            EtoroClient::new(
                "http://unused",
                EtoroKeys::new("a", "b"),
                Account::Demo,
                Duration::from_secs(1),
            ),
            vec![],
            Interval::H1,
            10,
            Duration::from_secs(600),
        )
    }

    fn ts(e: &MarketEvent) -> (String, i64) {
        match e {
            MarketEvent::Candle(c) | MarketEvent::Backfill(c) => (
                c.instrument.symbol().to_string(),
                c.open_time.as_millis() / 3_600_000,
            ),
            _ => unreachable!(),
        }
    }

    /// Warm-up as backfill in time order across instruments; a re-poll
    /// repeats bars already sent (skipped); a bar older than one already
    /// delivered (AAPL 3 after BTC 4) is dropped; a closed market adds
    /// nothing.
    #[test]
    fn bars_are_delivered_once_in_time_order() {
        let mut f = feed();
        f.take(vec![bar("BTC", 2), bar("AAPL", 1), bar("BTC", 1)], true);
        assert!(matches!(f.queue[0], MarketEvent::Backfill(_)));
        f.take(vec![bar("BTC", 2), bar("BTC", 3), bar("BTC", 4)], false);
        f.take(vec![bar("AAPL", 3)], false);
        f.take(vec![], false);
        let got: Vec<(String, i64)> = f.queue.iter().map(ts).collect();
        let want: Vec<(String, i64)> =
            [("AAPL", 1), ("BTC", 1), ("BTC", 2), ("BTC", 3), ("BTC", 4)]
                .iter()
                .map(|(s, i)| ((*s).to_owned(), *i))
                .collect();
        assert_eq!(got, want);
        assert!(matches!(f.queue[4], MarketEvent::Candle(_)));
    }
}
