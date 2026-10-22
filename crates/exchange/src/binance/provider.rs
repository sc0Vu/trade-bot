use super::{
    BinanceBroker, Credentials, DEMO_REST_URL, DEMO_WS_URL, KlineStream, LIVE_REST_URL,
    LIVE_WS_URL, PublicClient, ReqwestTransport, SignedClient, SystemClock, TESTNET_REST_URL,
    TESTNET_WS_URL,
};
use crate::{Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{AssetClass, Instrument, InstrumentId, Interval};
use serde::Deserialize;

/// Which Binance spot environment to trade on. Each takes its own keys:
/// a key from one is refused (`-2015`) by the others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// testnet.binance.vision: fake balances, thin testnet order book.
    /// The safe default.
    #[default]
    Testnet,
    /// demo.binance.com ("Demo Trading"): fake balances, prices that
    /// track mainnet.
    Demo,
    /// Mainnet: real money. Reached only through the CLI's live gate.
    Live,
}

impl Network {
    /// False only for [`Network::Live`]: both other networks trade fake
    /// balances.
    pub fn is_fake_money(self) -> bool {
        self != Self::Live
    }
}

/// Where and how to reach Binance spot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinanceConfig {
    pub network: Network,
    /// REST host override; default follows `network`.
    pub rest_url: Option<String>,
    /// WebSocket host override; default follows `network`.
    pub ws_url: Option<String>,
    /// Kline interval the feed subscribes to.
    pub interval: Interval,
}

impl BinanceConfig {
    pub fn rest_url(&self) -> &str {
        self.rest_url.as_deref().unwrap_or(match self.network {
            Network::Testnet => TESTNET_REST_URL,
            Network::Demo => DEMO_REST_URL,
            Network::Live => LIVE_REST_URL,
        })
    }

    pub fn ws_url(&self) -> &str {
        self.ws_url.as_deref().unwrap_or(match self.network {
            Network::Testnet => TESTNET_WS_URL,
            Network::Demo => DEMO_WS_URL,
            Network::Live => LIVE_WS_URL,
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
/// [`network`](Self::network).
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

    pub fn network(&self) -> Network {
        self.config.network
    }
}

#[async_trait]
impl Provider for BinanceSpotProvider {
    fn id(&self) -> &str {
        match self.config.network {
            Network::Testnet => "binance-testnet",
            Network::Demo => "binance-demo",
            Network::Live => "binance",
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

    fn cfg(network: Network) -> BinanceConfig {
        BinanceConfig {
            network,
            rest_url: None,
            ws_url: None,
            interval: Interval::M1,
        }
    }

    #[test]
    fn hosts_follow_network() {
        let t = cfg(Network::Testnet);
        assert_eq!(t.rest_url(), "https://testnet.binance.vision");
        assert_eq!(t.ws_url(), "wss://stream.testnet.binance.vision");
        let d = cfg(Network::Demo);
        assert_eq!(d.rest_url(), "https://demo-api.binance.com");
        assert_eq!(d.ws_url(), "wss://demo-stream.binance.com");
        let l = cfg(Network::Live);
        assert_eq!(l.rest_url(), "https://api.binance.com");
        assert_eq!(l.ws_url(), "wss://stream.binance.com:9443");
        assert!(Network::Testnet.is_fake_money() && Network::Demo.is_fake_money());
        assert!(!Network::Live.is_fake_money());
    }

    #[test]
    fn overrides_win() {
        let c = BinanceConfig {
            rest_url: Some("http://localhost:1".into()),
            ..cfg(Network::Testnet)
        };
        assert_eq!(c.rest_url(), "http://localhost:1");
        assert_eq!(c.ws_url(), "wss://stream.testnet.binance.vision");
    }

    #[test]
    fn provider_reports_network() {
        let p = BinanceSpotProvider::new(cfg(Network::Demo), Credentials::new("k", "s"), vec![]);
        assert_eq!(p.network(), Network::Demo);
        assert_eq!(p.id(), "binance-demo");
    }
}
