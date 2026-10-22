use super::{
    BinanceBroker, Credentials, KlineStream, LIVE_REST_URL, LIVE_WS_URL, PublicClient,
    ReqwestTransport, SignedClient, SystemClock, TESTNET_REST_URL, TESTNET_WS_URL,
};
use crate::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{AssetClass, Instrument, InstrumentId, Interval};

/// Where and how to reach Binance spot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinanceConfig {
    /// Testnet (fake balances) when true. The safe default everywhere.
    pub testnet: bool,
    /// REST host override; default follows `testnet`.
    pub rest_url: Option<String>,
    /// WebSocket host override; default follows `testnet`.
    pub ws_url: Option<String>,
    /// Kline interval the feed subscribes to.
    pub interval: Interval,
}

impl BinanceConfig {
    pub fn rest_url(&self) -> &str {
        self.rest_url.as_deref().unwrap_or(if self.testnet {
            TESTNET_REST_URL
        } else {
            LIVE_REST_URL
        })
    }

    pub fn ws_url(&self) -> &str {
        self.ws_url.as_deref().unwrap_or(if self.testnet {
            TESTNET_WS_URL
        } else {
            LIVE_WS_URL
        })
    }
}

/// Binance spot as a [`Provider`]: closed klines over WebSocket and a
/// [`BinanceBroker`] over signed REST, both against the hosts `config`
/// selects. The engine drives it exactly like the backtest provider
/// (invariant 3).
///
/// Whether this may run against mainnet is decided before it is built
/// (the CLI's live gate, invariant 7); the provider itself only reports
/// [`is_testnet`](Self::is_testnet).
pub struct BinanceSpotProvider {
    config: BinanceConfig,
    creds: Credentials,
    universe: Vec<InstrumentId>,
}

impl BinanceSpotProvider {
    /// `universe` is the whitelist: the symbols the feed subscribes to.
    pub fn new(config: BinanceConfig, creds: Credentials, universe: Vec<InstrumentId>) -> Self {
        Self {
            config,
            creds,
            universe,
        }
    }

    pub fn is_testnet(&self) -> bool {
        self.config.testnet
    }
}

#[async_trait]
impl Provider for BinanceSpotProvider {
    fn id(&self) -> &str {
        if self.config.testnet {
            "binance-testnet"
        } else {
            "binance"
        }
    }

    fn asset_classes(&self) -> &[AssetClass] {
        &[AssetClass::Spot]
    }

    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        let stream =
            KlineStream::connect(self.config.ws_url(), &self.universe, self.config.interval)
                .await?;
        Ok(Box::new(stream))
    }

    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let mut client = SignedClient::new(
            self.config.rest_url(),
            Box::new(ReqwestTransport::default()),
            self.creds.clone(),
            Box::new(SystemClock),
        );
        client.sync_time().await?;
        client.check_credentials().await?;
        let instruments = self
            .resolve(&InstrumentQuery::Ids(self.universe.clone()))
            .await?;
        Ok(Box::new(BinanceBroker::new(client, instruments)))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let public = PublicClient::new(self.config.rest_url());
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.venue().as_str() != "binance" {
                return Err(ExchangeError::UnknownInstrument(id.clone()));
            }
            out.push(public.instrument(id.symbol()).await?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(testnet: bool) -> BinanceConfig {
        BinanceConfig {
            testnet,
            rest_url: None,
            ws_url: None,
            interval: Interval::M1,
        }
    }

    #[test]
    fn hosts_follow_testnet_flag() {
        assert_eq!(cfg(true).rest_url(), "https://testnet.binance.vision");
        assert_eq!(cfg(true).ws_url(), "wss://stream.testnet.binance.vision");
        assert_eq!(cfg(false).rest_url(), "https://api.binance.com");
        assert_eq!(cfg(false).ws_url(), "wss://stream.binance.com:9443");
    }

    #[test]
    fn overrides_win() {
        let c = BinanceConfig {
            rest_url: Some("http://localhost:1".into()),
            ..cfg(true)
        };
        assert_eq!(c.rest_url(), "http://localhost:1");
        assert_eq!(c.ws_url(), "wss://stream.testnet.binance.vision");
    }

    #[test]
    fn provider_reports_network() {
        let p = BinanceSpotProvider::new(cfg(true), Credentials::new("k", "s"), vec![]);
        assert!(p.is_testnet());
        assert_eq!(p.id(), "binance-testnet");
    }
}
