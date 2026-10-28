//! Paper trading on MAX: MAX's live klines, the local [`PaperBroker`].
//!
//! MAX has no testnet, so a fake-money run can't send orders anywhere.
//! Instead the engine sees MAX's real closed bars (through the same
//! [`ReconnectingFeed`] as a live run) and trades against the paper broker
//! with the backtest's [`NextOpenFillModel`]: an order decided on a bar
//! fills at the next bar's open, with the stated fee and slippage. Paper
//! and backtest on MAX are therefore the same model on different data
//! (invariant 3). No API key is read and nothing reaches MAX but public
//! data requests.
//!
//! The broker holds its balance and positions in memory: every run starts
//! from `initial_cash` and flat.

use super::public::MaxPublicClient;
use super::ws::{MaxRestKlines, MaxWsConnector};
use crate::binance::feed::ReconnectingFeed;
use crate::paper::{BpsSlippage, NextOpenFillModel, PaperBroker, RateFee};
use crate::rate_limit::RateLimiter;
use crate::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{AssetClass, Instrument, InstrumentId, Interval, Notional};
use std::sync::Arc;
use std::time::Duration;

/// How a MAX paper run trades.
#[derive(Clone, Debug)]
pub struct MaxPaperConfig {
    /// Kline interval the feed subscribes to (1m or longer).
    pub interval: Interval,
    /// Closed bars per coin fetched on start (the engine's `history_len`).
    pub warmup: usize,
    /// How long the feed keeps reconnecting before the run stops.
    pub max_outage: Duration,
    /// Longest one REST request may take.
    pub http_timeout: Duration,
    /// Paper fees (maker and taker rates, in the received asset).
    pub fees: RateFee,
    /// Paper taker slippage.
    pub slippage: BpsSlippage,
    /// Starting balance in the first instrument's quote asset.
    pub initial_cash: Notional,
}

/// See the module docs.
pub struct MaxPaperProvider {
    config: MaxPaperConfig,
    universe: Vec<InstrumentId>,
    /// One budget for every request to MAX's host (per IP).
    limiter: Arc<RateLimiter>,
}

impl MaxPaperProvider {
    /// `universe` is the whitelist, all `max:` instruments.
    pub fn new(config: MaxPaperConfig, universe: Vec<InstrumentId>) -> Self {
        Self {
            config,
            universe,
            limiter: Arc::new(RateLimiter::new(super::default_limits())),
        }
    }

    fn client(&self) -> MaxPublicClient {
        MaxPublicClient::with_timeout(super::REST_URL, self.config.http_timeout)
            .with_limiter(self.limiter.clone())
    }
}

#[async_trait]
impl Provider for MaxPaperProvider {
    fn id(&self) -> &str {
        "max-paper"
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        Ok(Box::new(ReconnectingFeed::new(
            Box::new(MaxWsConnector {
                url: super::WS_URL.to_owned(),
                ids: self.universe.clone(),
                interval: self.config.interval,
            }),
            Box::new(MaxRestKlines {
                client: self.client(),
                interval: self.config.interval,
            }),
            self.universe.clone(),
            self.config.warmup,
            self.config.max_outage,
        )))
    }

    /// A fresh [`PaperBroker`] over the universe's MAX filters, trading
    /// from `initial_cash` in the first instrument's quote.
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let instruments: Vec<Instrument> = self
            .resolve(&InstrumentQuery::Ids(self.universe.clone()))
            .await?;
        let quote = instruments
            .first()
            .map(|i| i.quote.clone())
            .ok_or_else(|| ExchangeError::Decode("empty universe".into()))?;
        Ok(Box::new(
            PaperBroker::new(
                instruments,
                Box::new(NextOpenFillModel),
                Box::new(self.config.slippage),
                Box::new(self.config.fees),
            )
            .with_cash(quote, self.config.initial_cash),
        ))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let client = self.client();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.venue().as_str() != super::VENUE {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            out.push(client.instrument(id.symbol()).await?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::FeeAsset;
    use rust_decimal_macros::dec;

    fn provider(universe: &[&str]) -> MaxPaperProvider {
        MaxPaperProvider::new(
            MaxPaperConfig {
                interval: Interval::M1,
                warmup: 5,
                max_outage: Duration::from_secs(60),
                http_timeout: Duration::from_secs(10),
                fees: RateFee::new(dec!(0.0015), dec!(0.0015), FeeAsset::Received).unwrap(),
                slippage: BpsSlippage::new(dec!(20)).unwrap(),
                initial_cash: Notional::new(dec!(1000)),
            },
            universe.iter().map(|s| s.parse().unwrap()).collect(),
        )
    }

    #[tokio::test]
    async fn refuses_instruments_of_another_venue() {
        let p = provider(&["binance:BTCUSDT"]);
        assert_eq!(p.id(), "max-paper");
        let r = p
            .resolve(&InstrumentQuery::Ids(vec![
                "binance:BTCUSDT".parse().unwrap(),
            ]))
            .await;
        assert!(matches!(r, Err(ExchangeError::UnknownInstrument(_))));
    }

    /// Against MAX itself (run with `--ignored`): the broker starts flat
    /// with the configured cash, on MAX's own filters.
    #[tokio::test]
    #[ignore = "network: live MAX"]
    async fn broker_starts_flat_on_max_filters() {
        let p = provider(&["max:BTCTWD"]);
        let b = p.broker().await.unwrap();
        let id: InstrumentId = "max:BTCTWD".parse().unwrap();
        assert!(b.position(&id).is_flat());
        let i = &p.resolve(&InstrumentQuery::Ids(vec![id])).await.unwrap()[0];
        assert_eq!(i.quote.as_str(), "TWD");
        assert_eq!(i.lot_size.get(), dec!(0.00000001));
    }
}
