use super::feed::{ReconnectingFeed, RestKlines, WsConnector};
use super::limits;
use super::reconcile::{Orphans, Phase, Reconciled, reconcile};
use super::{
    BinanceBroker, Clock, Credentials, DEMO_REST_URL, DEMO_WS_API_URL, DEMO_WS_URL, LIVE_REST_URL,
    LIVE_WS_API_URL, LIVE_WS_URL, PublicClient, ReqwestTransport, SignedClient, SystemClock,
    TESTNET_REST_URL, TESTNET_WS_API_URL, TESTNET_WS_URL, UserStream, run_tag,
};
use crate::rate_limit::RateLimiter;
use crate::{Books, Broker, ExchangeError, InstrumentQuery, MarketData, Provider};
use async_trait::async_trait;
use bot_core::{AssetClass, Instrument, InstrumentId, Interval};
use serde::Deserialize;
use std::sync::Arc;

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
    /// Longest one REST request may take ([`super::DEFAULT_HTTP_TIMEOUT`]
    /// unless configured).
    pub http_timeout: std::time::Duration,
    /// Closed bars per coin fetched on start to warm up history (the
    /// engine's `history_len`).
    pub warmup: usize,
    /// How long the feed keeps reconnecting before the run stops.
    pub max_outage: std::time::Duration,
    /// Subscribe to the user data stream so the broker polls only orders
    /// that changed ([`super::user_stream`]). Ignored while `rest_url` is
    /// overridden: the stream would watch the network's real host, not
    /// the override's orders.
    pub user_stream: bool,
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

    /// The WebSocket API host the user data stream connects to, or `None`
    /// when the stream is off (see [`user_stream`](Self::user_stream)).
    pub fn ws_api_url(&self) -> Option<&str> {
        (self.user_stream && self.rest_url.is_none()).then_some(match self.network {
            Network::Testnet => TESTNET_WS_API_URL,
            Network::Demo => DEMO_WS_API_URL,
            Network::Live => LIVE_WS_API_URL,
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
    books: Arc<dyn Books>,
    /// One budget for every client of this host: Binance counts weight per
    /// IP, whether the call comes from the broker, a lookup or the exit
    /// close-out.
    limiter: Arc<RateLimiter>,
    /// Prefix of this run's client order ids ([`run_tag`]).
    run_tag: String,
    /// What startup does with this bot's orders the books don't know.
    orphans: Orphans,
}

impl BinanceSpotProvider {
    /// `universe` is the whitelist: the symbols the feed subscribes to.
    /// `books` holds this network's orders, fills and holdings; it must not
    /// be shared with another network (`SqlStore::bind_network`).
    pub fn new(
        config: BinanceConfig,
        creds: Credentials,
        universe: Vec<InstrumentId>,
        books: Arc<dyn Books>,
    ) -> Self {
        Self {
            config,
            creds,
            universe,
            books,
            limiter: Arc::new(RateLimiter::new(limits::default_limits())),
            run_tag: run_tag(SystemClock.now_millis()),
            orphans: Orphans::Refuse,
        }
    }

    /// Sets what startup does with orphans ([`Orphans`]); refusing them is
    /// the default.
    pub fn with_orphans(mut self, orphans: Orphans) -> Self {
        self.orphans = orphans;
        self
    }

    /// This run's client order id prefix: order lines log the full
    /// `<tag>-o-N` the venue holds.
    pub fn run_tag(&self) -> &str {
        &self.run_tag
    }

    pub fn network(&self) -> Network {
        self.config.network
    }

    /// Cancels every order the books still have open on Binance and records
    /// their last fills ([`Phase::Exit`]). Call it however a run ends:
    /// orders left working have no strategy watching them.
    pub async fn close_out(&self) -> Result<Reconciled, ExchangeError> {
        let client = self.client().await?;
        let instruments = self.books_instruments().await?;
        reconcile(&client, &instruments, &*self.books, Phase::Exit).await
    }

    async fn client(&self) -> Result<SignedClient, ExchangeError> {
        let mut client = SignedClient::new(
            self.config.rest_url(),
            Box::new(ReqwestTransport::new(self.config.http_timeout)?),
            self.creds.clone(),
            Box::new(SystemClock),
        )
        .with_limiter(self.limiter.clone());
        client.sync_time().await?;
        Ok(client)
    }

    /// The whitelist plus any instrument an unfinished journal order is on
    /// (the whitelist may have changed since it was placed).
    async fn books_instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let mut ids = self.universe.clone();
        for o in self.books.unfinished_orders().await? {
            if !ids.contains(&o.instrument) {
                ids.push(o.instrument);
            }
        }
        self.resolve(&InstrumentQuery::Ids(ids)).await
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

    /// The kline WebSocket inside a [`ReconnectingFeed`]: warmed up with
    /// `warmup` bars of history, reconnecting for up to `max_outage`.
    async fn market_data(&self) -> Result<Box<dyn MarketData>, ExchangeError> {
        let connector = WsConnector {
            url: self.config.ws_url().to_owned(),
            ids: self.universe.clone(),
            interval: self.config.interval,
        };
        let source = RestKlines {
            client: PublicClient::with_timeout(self.config.rest_url(), self.config.http_timeout)
                .with_limiter(self.limiter.clone()),
            interval: self.config.interval,
        };
        Ok(Box::new(ReconnectingFeed::new(
            Box::new(connector),
            Box::new(source),
            self.universe.clone(),
            self.config.warmup,
            self.config.max_outage,
        )))
    }

    /// Checks the key, then reconciles the books with Binance
    /// ([`Phase::Startup`], orphans as [`with_orphans`](Self::with_orphans)) before the broker starts from them.
    async fn broker(&self) -> Result<Box<dyn Broker>, ExchangeError> {
        let client = self.client().await?;
        client.check_credentials().await?;
        let books_instruments = self.books_instruments().await?;
        let r = reconcile(
            &client,
            &books_instruments,
            &*self.books,
            Phase::Startup(self.orphans),
        )
        .await?;
        tracing::info!(
            orders_closed = r.orders_closed,
            fills_recovered = r.fills_recovered,
            "reconciled with Binance"
        );
        let instruments = books_instruments
            .into_iter()
            .filter(|i| self.universe.contains(&i.id))
            .collect();
        let offset_ms = client.offset_ms();
        let broker = BinanceBroker::open(
            client,
            instruments,
            self.run_tag.clone(),
            self.books.clone(),
        )
        .await?
        .with_recovered(r.fills);
        Ok(Box::new(match self.config.ws_api_url() {
            Some(url) => broker.with_user_stream(UserStream::spawn(
                url.to_owned(),
                self.creds.clone(),
                Box::new(SystemClock),
                offset_ms,
                Default::default(),
            )),
            None => broker,
        }))
    }

    async fn resolve(&self, query: &InstrumentQuery) -> Result<Vec<Instrument>, ExchangeError> {
        let InstrumentQuery::Ids(ids) = query;
        let public = PublicClient::with_timeout(self.config.rest_url(), self.config.http_timeout)
            .with_limiter(self.limiter.clone());
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
            http_timeout: crate::binance::DEFAULT_HTTP_TIMEOUT,
            warmup: 0,
            max_outage: std::time::Duration::from_secs(600),
            user_stream: true,
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
        assert_eq!(
            t.ws_api_url(),
            Some("wss://ws-api.testnet.binance.vision/ws-api/v3")
        );
        assert_eq!(
            d.ws_api_url(),
            Some("wss://demo-ws-api.binance.com/ws-api/v3")
        );
        assert_eq!(
            l.ws_api_url(),
            Some("wss://ws-api.binance.com:443/ws-api/v3")
        );
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
        // The stream would watch testnet, not the override's orders.
        assert_eq!(c.ws_api_url(), None);
        let off = BinanceConfig {
            user_stream: false,
            ..cfg(Network::Testnet)
        };
        assert_eq!(off.ws_api_url(), None);
    }

    #[tokio::test]
    async fn provider_reports_network() {
        let db = crate::test_db::TempDb::new().await;
        let p = BinanceSpotProvider::new(
            cfg(Network::Demo),
            Credentials::new("k", "s"),
            vec![],
            db.store.clone(),
        );
        assert_eq!(p.network(), Network::Demo);
        assert_eq!(p.id(), "binance-demo");
        assert!(
            p.run_tag().starts_with(crate::binance::CLIENT_ID_PREFIX),
            "{}",
            p.run_tag()
        );
    }
}
