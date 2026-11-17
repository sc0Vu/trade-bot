//! Paper trading on Binance USDⓈ-M perpetuals: the venue's live futures
//! klines and funding, the local [`PerpPaperBroker`].
//!
//! Nothing is sent to the venue but public data requests: an order decided
//! on a bar fills at the next bar's open with the stated fee and slippage
//! (the backtest's [`NextOpenFillModel`]), margin and liquidation are the
//! paper broker's. Paper and backtest are the same model on different data
//! (invariant 3). The broker lives in memory: every run starts flat from
//! `initial_cash`.
//!
//! Funding comes from [`FundingWatch`]: every `every` (default 5 min) it
//! asks each symbol's funding history for settlements after the last one
//! seen (the first poll starts from the run's start: nothing is replayed)
//! and delivers each as `MarketEvent::Funding` before the next market
//! event, stamped with that event's time (the feed never runs backwards; a
//! settlement is learnt after it happens). A record without a mark price
//! is skipped with a warning (fresh records carry one). A failed poll is a
//! warning and retried next time.

use super::PublicClient;
use super::feed::{ReconnectingFeed, RestKlines, WsConnector};
use super::usdm::{self, FundingRate};
use crate::paper::{BpsSlippage, NextOpenFillModel, PerpPaperBroker, PerpTerms, RateFee};
use crate::rate_limit::RateLimiter;
use crate::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{
    AssetClass, Decimal, Instrument, InstrumentId, Interval, MarketEvent, Notional, Symbol,
    Timestamp,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Public futures kline stream (production; paper reads real prices).
/// Klines are a "market" stream: `/market/stream?streams=…`. The legacy
/// `/stream` path still accepts the connection but sends no klines
/// (checked 2026-11-17), which the feed's stall check would only catch
/// after two bars.
pub const WS_URL: &str = "wss://fstream.binance.com/market";
/// How often [`FundingWatch`] asks for new settlements.
pub const FUNDING_POLL: Duration = Duration::from_secs(300);

/// Where [`FundingWatch`] reads settlements from.
#[async_trait]
pub trait FundingSource: Send + Sync {
    /// Settlements of `symbol` with `ts > after`, oldest first.
    async fn after(
        &self,
        symbol: &Symbol,
        after: Timestamp,
    ) -> Result<Vec<FundingRate>, ExchangeError>;
}

#[async_trait]
impl FundingSource for PublicClient {
    async fn after(
        &self,
        symbol: &Symbol,
        after: Timestamp,
    ) -> Result<Vec<FundingRate>, ExchangeError> {
        let now = now_millis();
        self.funding_rates(
            symbol,
            Timestamp::from_millis(after.as_millis() + 1),
            Timestamp::from_millis(now + 1),
        )
        .await
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A feed that also delivers funding settlements; see the module docs.
pub struct FundingWatch {
    inner: Box<dyn MarketData>,
    source: Box<dyn FundingSource>,
    universe: Vec<InstrumentId>,
    every: Duration,
    last_poll: Option<Instant>,
    /// Newest settlement seen per instrument (starts at the run's start).
    seen: HashMap<InstrumentId, Timestamp>,
    queue: VecDeque<MarketEvent>,
}

impl FundingWatch {
    /// Watches `universe`'s funding from `start` on (settlements at or
    /// before it are never delivered).
    pub fn new(
        inner: Box<dyn MarketData>,
        source: Box<dyn FundingSource>,
        universe: Vec<InstrumentId>,
        start: Timestamp,
        every: Duration,
    ) -> Self {
        let seen = universe.iter().map(|id| (id.clone(), start)).collect();
        Self {
            inner,
            source,
            universe,
            every,
            last_poll: None,
            seen,
            queue: VecDeque::new(),
        }
    }

    async fn poll(&mut self, ts: Timestamp) {
        for id in &self.universe {
            let after = self.seen[id];
            match self.source.after(id.symbol(), after).await {
                Ok(rates) => {
                    for r in rates.into_iter().filter(|r| r.ts > after) {
                        self.seen.insert(id.clone(), r.ts);
                        let Some(mark) = r.mark else {
                            tracing::warn!(instrument = %id, "funding settlement without a mark price; skipped");
                            continue;
                        };
                        self.queue.push_back(MarketEvent::Funding {
                            instrument: id.clone(),
                            rate: r.rate,
                            mark,
                            ts,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(instrument = %id, error = %e, "funding history unavailable; retrying at the next poll")
                }
            }
        }
    }
}

#[async_trait]
impl MarketData for FundingWatch {
    async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
        if let Some(e) = self.queue.pop_front() {
            return Ok(Some(e));
        }
        let Some(event) = self.inner.next().await? else {
            return Ok(None);
        };
        if self.last_poll.is_none_or(|t| t.elapsed() >= self.every) {
            self.last_poll = Some(Instant::now());
            self.poll(event.ts()).await;
        }
        if self.queue.is_empty() {
            return Ok(Some(event));
        }
        self.queue.push_back(event);
        Ok(self.queue.pop_front())
    }
}

/// How a USDⓈ-M paper run trades.
#[derive(Clone, Debug)]
pub struct UsdmPaperConfig {
    pub interval: Interval,
    /// Closed bars per coin fetched on start (the engine's `history_len`).
    pub warmup: usize,
    pub max_outage: Duration,
    pub http_timeout: Duration,
    /// Paper fees (maker and taker), paid in the quote.
    pub fees: RateFee,
    pub slippage: BpsSlippage,
    /// The paper wallet, in the quote.
    pub initial_cash: Notional,
    /// Leverage, margin mode and the account's brackets per instrument.
    pub terms: HashMap<InstrumentId, PerpTerms>,
    /// Clearance fee rate; `None`: the highest of the symbols' published
    /// `liquidationFee`.
    pub clearance: Option<Decimal>,
}

/// See the module docs.
pub struct UsdmPaperProvider {
    config: UsdmPaperConfig,
    universe: Vec<InstrumentId>,
    limiter: Arc<RateLimiter>,
}

impl UsdmPaperProvider {
    /// `universe` is the whitelist, all `binance-usdm:` instruments with
    /// terms in `config`.
    pub fn new(config: UsdmPaperConfig, universe: Vec<InstrumentId>) -> Self {
        Self {
            config,
            universe,
            limiter: Arc::new(RateLimiter::new(super::limits::default_limits())),
        }
    }

    fn client(&self) -> PublicClient {
        let mut c = PublicClient::usdm(usdm::LIVE_URL).with_limiter(self.limiter.clone());
        c.set_timeout(self.config.http_timeout);
        c
    }
}

#[async_trait]
impl Provider for UsdmPaperProvider {
    fn id(&self) -> &str {
        "binance-usdm-paper"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Perpetual]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        let feed = ReconnectingFeed::new(
            Box::new(WsConnector {
                url: WS_URL.to_owned(),
                ids: self.universe.clone(),
                interval: self.config.interval,
            }),
            Box::new(RestKlines {
                client: self.client(),
                interval: self.config.interval,
            }),
            self.universe.clone(),
            self.config.warmup,
            self.config.max_outage,
        );
        Ok(Box::new(FundingWatch::new(
            Box::new(feed),
            Box::new(self.client()),
            self.universe.clone(),
            Timestamp::from_millis(now_millis()),
            FUNDING_POLL,
        )))
    }

    /// A fresh [`PerpPaperBroker`] over today's filters and the configured
    /// terms, from `initial_cash`.
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let client = self.client();
        let mut instruments = Vec::with_capacity(self.universe.len());
        let mut clearance = Decimal::ZERO;
        for id in &self.universe {
            let info = client.perp_instrument(id.symbol()).await?;
            clearance = clearance.max(info.liquidation_fee);
            instruments.push(info.instrument);
        }
        Ok(Box::new(PerpPaperBroker::new(
            instruments,
            self.config.terms.clone(),
            Box::new(NextOpenFillModel),
            Box::new(self.config.slippage),
            Box::new(self.config.fees),
            self.config.initial_cash,
            self.config.clearance.unwrap_or(clearance),
        )?))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let client = self.client();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.venue().as_str() != usdm::VENUE {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            out.push(client.perp_instrument(id.symbol()).await?.instrument);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::Price;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    struct Feed(VecDeque<MarketEvent>);

    #[async_trait]
    impl MarketData for Feed {
        async fn next(&mut self) -> Result<Option<MarketEvent>, ExchangeError> {
            Ok(self.0.pop_front())
        }
    }

    /// Answers each poll with the next scripted list (all symbols alike),
    /// recording the `after` asked for.
    struct Script {
        answers: Mutex<VecDeque<Vec<FundingRate>>>,
        asked: Arc<Mutex<Vec<i64>>>,
    }

    #[async_trait]
    impl FundingSource for Script {
        async fn after(
            &self,
            _: &Symbol,
            after: Timestamp,
        ) -> Result<Vec<FundingRate>, ExchangeError> {
            self.asked.lock().unwrap().push(after.as_millis());
            Ok(self.answers.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    fn id() -> InstrumentId {
        "binance-usdm:BTCUSDT".parse().unwrap()
    }

    fn tick(ts: i64) -> MarketEvent {
        MarketEvent::SessionOpen {
            venue: "binance-usdm".parse().unwrap(),
            ts: Timestamp::from_millis(ts),
        }
    }

    fn rate(ts: i64, mark: Option<&str>) -> FundingRate {
        FundingRate {
            ts: Timestamp::from_millis(ts),
            rate: dec!(0.0001),
            mark: mark.map(|m| m.parse().unwrap()),
        }
    }

    /// Start 1,000. Poll 1 (with the event at 5,000) returns a settlement
    /// at 900 (before the start: dropped), one at 2,000 (delivered first,
    /// stamped 5,000) and one at 3,000 without a mark (skipped, but seen).
    /// Poll 2 (every event: `every` 0) asks after 3,000 and gets nothing.
    #[tokio::test]
    async fn delivers_new_settlements_before_the_next_event_once_each() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let script = Script {
            answers: Mutex::new(VecDeque::from([vec![
                rate(900, Some("10")),
                rate(2000, Some("100.5")),
                rate(3000, None),
            ]])),
            asked: asked.clone(),
        };
        let mut w = FundingWatch::new(
            Box::new(Feed(VecDeque::from([tick(5000), tick(6000)]))),
            Box::new(script),
            vec![id()],
            Timestamp::from_millis(1000),
            Duration::ZERO,
        );
        let mut out = Vec::new();
        while let Some(e) = w.next().await.unwrap() {
            out.push(e);
        }
        assert_eq!(
            out,
            vec![
                MarketEvent::Funding {
                    instrument: id(),
                    rate: dec!(0.0001),
                    mark: Price::new(dec!(100.5)),
                    ts: Timestamp::from_millis(5000),
                },
                tick(5000),
                tick(6000),
            ]
        );
        assert_eq!(*asked.lock().unwrap(), vec![1000, 3000]);
    }
}
