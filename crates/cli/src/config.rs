//! Run configuration: `config/config.toml` (or `--config` / `BOT_CONFIG`),
//! then `BOT_*` environment variables (`BOT_EXCHANGE__NETWORK=demo`),
//! into one typed struct.
//!
//! `deny_unknown_fields` everywhere: a typo is an error, never a silent
//! default, and there is no field a secret could go in, so `api_key = ...`
//! in a file fails to load (invariant 6). Keys come only from
//! `BINANCE_API_KEY` / `BINANCE_API_SECRET`.

use anyhow::{Context, bail};
use bot_core::{Decimal, InstrumentId, Interval, Notional, VenueId};
use exchange::binance::Network;
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default config path, relative to the working directory.
pub const DEFAULT_PATH: &str = "config/config.toml";

/// What the config intends. Live also needs `exchange.network = "live"` and
/// the `--live --i-understand` flags; see [`crate::gate`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Paper,
    Live,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    pub exchange: ExchangeSection,
    /// The only instruments the bot may trade.
    pub whitelist: Vec<InstrumentId>,
    pub strategy: StrategySection,
    #[serde(default)]
    pub engine: EngineSection,
    #[serde(default)]
    pub db: DbSection,
    #[serde(default)]
    pub metrics: MetricsSection,
    #[serde(default)]
    pub api: ApiSection,
    #[serde(default)]
    pub log: LogSection,
    /// Keep a share of the quote cash in Binance Simple Earn (live only).
    #[serde(default)]
    pub earn: EarnSection,
    /// How a paper run on a venue without a testnet (MAX) fills orders.
    /// Required with `exchange.venue = "max"`; unused on Binance, whose
    /// paper runs trade on its testnet or demo network.
    pub paper: Option<PaperSection>,
    /// Margin terms of a perpetual paper run (`exchange.venue =
    /// "binance-usdm"`, required there).
    pub perp: Option<PerpSection>,
}

/// `[perp]`: how a USDⓈ-M paper run margins its positions.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerpSection {
    /// Leverage for every whitelisted symbol (≥ 1; refused above what the
    /// account's brackets allow).
    pub leverage: u32,
    /// `"isolated"` (default) or `"cross"`.
    #[serde(default)]
    pub margin: bot_core::margin::MarginMode,
    /// Which network's futures host the account's margin brackets are read
    /// from (a signed call; `BINANCE_API_KEY` / `BINANCE_API_SECRET` must
    /// belong to it). Default `demo`.
    #[serde(default = "demo")]
    pub brackets_from: Network,
    /// Liquidation clearance fee rate (`"0.0125"`); default the highest
    /// published `liquidationFee` of the symbols.
    #[serde(default, deserialize_with = "opt_exact_decimal")]
    pub clearance: Option<Decimal>,
}

fn demo() -> Network {
    Network::Demo
}

/// Which exchange the bot trades on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    /// Binance spot (`binance:BTCUSDT`): testnet, demo or mainnet.
    #[default]
    Binance,
    /// MAX by MaiCoin (`max:BTCUSDT`, `max:BTCTWD`). No testnet: paper only
    /// (live MAX data, local fills) until live orders are built.
    Max,
    /// Binance USDⓈ-M perpetual futures (`binance-usdm:BTCUSDT`): backtest,
    /// `fetch-data`, and paper runs (live futures prices, local fills and
    /// margin, `[paper]` + `[perp]`). Live orders are refused until margin
    /// is handled against the venue.
    #[serde(rename = "binance-usdm")]
    #[value(name = "binance-usdm")]
    BinanceUsdm,
}

impl Venue {
    /// The venue part of its instrument ids.
    pub fn id(self) -> VenueId {
        let name = match self {
            Self::Binance => "binance",
            Self::Max => exchange::max::VENUE,
            Self::BinanceUsdm => exchange::binance::usdm::VENUE,
        };
        VenueId::new(name).unwrap_or_else(|_| unreachable!("valid venue id"))
    }
}

/// The local paper broker's settings (`[paper]`), as in `backtest`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperSection {
    /// Starting balance in the first whitelisted instrument's quote asset
    /// (`"1000"`).
    pub initial_cash: Notional,
    /// Maker and taker fee as a fraction (`"0.0015"`), charged in the
    /// received asset.
    #[serde(deserialize_with = "exact_decimal")]
    pub fee: Decimal,
    /// Taker slippage in basis points (`"20"`). Required, like
    /// `backtest --slippage-bps`: a market order never fills at the price
    /// the decision saw.
    #[serde(deserialize_with = "exact_decimal")]
    pub slippage_bps: Decimal,
}

/// `[earn]`: keep `ratio` of the quote cash in Binance Simple Earn.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarnSection {
    /// Off unless set.
    #[serde(default)]
    pub enabled: bool,
    /// Share of the quote cash (spot free + Earn) kept in Earn (`"0.1"` =
    /// 10%). In (0, 1). Required when enabled.
    #[serde(default, deserialize_with = "opt_exact_decimal")]
    pub ratio: Option<Decimal>,
    /// Move only when off by more than this share of the cash (`"0.01"`,
    /// the default), so small moves don't churn. In [0, 0.5).
    #[serde(default = "one_percent", deserialize_with = "exact_decimal")]
    pub band: Decimal,
    /// `flexible` (the default; redeemable any time) or `locked` (a
    /// `locked_days` term: only topped up, never redeemed early).
    #[serde(default)]
    pub product: EarnProductKind,
    /// Term of the locked product. Required with `product = "locked"`.
    pub locked_days: Option<u32>,
    /// Minutes between sweeps (default 60).
    #[serde(default = "sixty")]
    pub interval_minutes: u64,
}

/// `[earn] product`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EarnProductKind {
    #[default]
    Flexible,
    Locked,
}

fn one_percent() -> Decimal {
    Decimal::new(1, 2)
}

fn sixty() -> u64 {
    60
}

impl Default for EarnSection {
    fn default() -> Self {
        Self {
            enabled: false,
            ratio: None,
            band: one_percent(),
            product: EarnProductKind::Flexible,
            locked_days: None,
            interval_minutes: sixty(),
        }
    }
}

impl EarnSection {
    /// The sweep settings, if enabled and valid (see `Config::validate`).
    pub fn to_exchange(&self) -> Option<exchange::binance::earn::EarnConfig> {
        if !self.enabled {
            return None;
        }
        Some(exchange::binance::earn::EarnConfig {
            ratio: self.ratio?,
            band: self.band,
            product: match self.product {
                EarnProductKind::Flexible => exchange::binance::earn::EarnProduct::Flexible,
                EarnProductKind::Locked => exchange::binance::earn::EarnProduct::Locked {
                    days: self.locked_days?,
                },
            },
            every: std::time::Duration::from_secs(self.interval_minutes.saturating_mul(60)),
        })
    }
}

fn opt_exact_decimal<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
    exact_decimal(d).map(Some)
}

/// A decimal from a string only (`"0.0015"`), never a TOML float
/// (invariant 1).
fn exact_decimal<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    let s = String::deserialize(d)?;
    Decimal::from_str_exact(&s).map_err(serde::de::Error::custom)
}

/// Logging for `run`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSection {
    /// A `tracing` filter, the syntax `RUST_LOG` takes (`"info,sqlx=warn"`,
    /// `"info,exchange=debug"`). Unset: `info,sqlx=warn`. `RUST_LOG`, when
    /// set, overrides it.
    pub level: Option<String>,
    /// How the console shows log lines. Default `text`.
    #[serde(default)]
    pub format: LogFormat,
    /// Also append every log line, as JSON, to this file: the one a log
    /// shipper (Grafana Alloy, Vector, the Datadog agent) tails. Unset: no
    /// file. Not rotated by the bot; rotate with `copytruncate`.
    pub file: Option<PathBuf>,
}

/// Console log format.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line, the same shape as `log.file`'s: for a
    /// container or supervisor that collects stdout.
    Json,
}

/// The log filter for `run`: `RUST_LOG` if set and valid, else the
/// configured `log.level`, else `default`.
pub fn log_filter(rust_log: Option<String>, configured: Option<&str>, default: &str) -> String {
    rust_log
        .filter(|f| tracing_subscriber::EnvFilter::try_new(f).is_ok())
        .or_else(|| configured.map(str::to_owned))
        .unwrap_or_else(|| default.to_owned())
}

/// Env var holding the API's bearer token. Not `BOT_*`: that prefix is the
/// config layer, where it would be read as an `api_token` field. Secrets
/// come only from the environment (invariant 6).
pub const API_TOKEN_VAR: &str = "TRADEBOT_API_TOKEN";

/// The read-only HTTP API for `run` (see the `api` crate).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    /// Serve the API. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Address to serve on. Default `127.0.0.1:9185`. A non-loopback
    /// address also needs `allow_public` and a token in
    /// `TRADEBOT_API_TOKEN`.
    #[serde(default = "api_listen")]
    pub listen: std::net::SocketAddr,
    /// Permit a non-loopback `listen`. Put TLS (a reverse proxy) in front:
    /// the token travels in a header.
    #[serde(default)]
    pub allow_public: bool,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: api_listen(),
            allow_public: false,
        }
    }
}

fn api_listen() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], 9185))
}

/// The token the API must require, from `TRADEBOT_API_TOKEN`'s value
/// (`env`). A blank value counts as unset. Off loopback a token is
/// mandatory: the API shows the account's books.
pub fn api_token(api: &ApiSection, env: Option<String>) -> anyhow::Result<Option<String>> {
    let token = env.filter(|t| !t.trim().is_empty());
    if !api.listen.ip().is_loopback() && token.is_none() {
        bail!(
            "api.listen {} is not loopback; set {API_TOKEN_VAR} so requests need a token",
            api.listen
        );
    }
    Ok(token)
}

/// Prometheus metrics for `run` (see `telemetry`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSection {
    /// Serve `/metrics`. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Address to serve on. Default `127.0.0.1:9184`. The endpoint has no
    /// authentication, so a non-loopback address also needs
    /// `allow_public`.
    #[serde(default = "metrics_listen")]
    pub listen: std::net::SocketAddr,
    /// Also export positions, cost basis and realized P&L. Off by default:
    /// it puts the account's books on an unauthenticated endpoint.
    #[serde(default)]
    pub include_money: bool,
    /// Permit a non-loopback `listen`. Put a firewall or an authenticating
    /// proxy in front if you set it.
    #[serde(default)]
    pub allow_public: bool,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: metrics_listen(),
            include_money: false,
            allow_public: false,
        }
    }
}

fn metrics_listen() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], 9184))
}

/// Where a live run keeps its books (orders, fills, holdings).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbSection {
    /// Database URL. Default: one SQLite file per network,
    /// `data/<network>.db`. The database is stamped with its network on
    /// first use, and opening it for another network is refused.
    pub url: Option<String>,
}

impl DbSection {
    /// The configured URL, or the per-network default.
    pub fn url_for(&self, network: Network) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| format!("sqlite://data/{}.db?mode=rwc", network_name(network)))
    }
}

/// The name a network goes by in config, file names and the database
/// stamp.
pub fn network_name(network: Network) -> &'static str {
    match network {
        Network::Testnet => "testnet",
        Network::Demo => "demo",
        Network::Live => "live",
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeSection {
    /// `binance` (default) or `max`.
    #[serde(default)]
    pub venue: Venue,
    /// Binance only: `testnet` (default), `demo` or `live`. Keys must
    /// belong to it.
    #[serde(default)]
    pub network: Network,
    pub rest_url: Option<String>,
    pub ws_url: Option<String>,
    #[serde(default = "one_minute")]
    pub interval: Interval,
    /// Longest one Binance REST request may take, in seconds. Default 10.
    #[serde(default = "ten")]
    pub http_timeout_secs: u64,
    /// How long the market-data feed keeps reconnecting before the run
    /// stops, in seconds. Default 600.
    #[serde(default = "six_hundred")]
    pub max_outage_secs: u64,
    /// Watch the user data stream so only orders that changed are polled
    /// (all are still polled every minute). Default true; off, or with
    /// `rest_url` set, every open order is polled every event.
    #[serde(default = "yes")]
    pub user_stream: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySection {
    pub name: String,
    /// Strategy-specific; each strategy checks its own params at build.
    #[serde(default)]
    pub params: toml::Table,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSection {
    #[serde(default = "five_hundred")]
    pub history_len: usize,
    /// Hours before a scheduled delisting at which the engine itself sells
    /// the holding (and cancels orders on the coin). New buys are refused
    /// from the announcement on. Default 24.
    #[serde(default = "twenty_four")]
    pub delist_exit_hours: u64,
}

fn twenty_four() -> u64 {
    24
}

impl EngineSection {
    /// [`delist_exit_hours`](Self::delist_exit_hours) as a duration.
    pub fn delist_exit(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.delist_exit_hours.saturating_mul(3600))
    }
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            history_len: five_hundred(),
            delist_exit_hours: twenty_four(),
        }
    }
}

fn one_minute() -> Interval {
    Interval::M1
}
fn five_hundred() -> usize {
    500
}
fn yes() -> bool {
    true
}
fn ten() -> u64 {
    10
}
fn six_hundred() -> u64 {
    600
}

impl Config {
    /// Loads `path` (must exist if given explicitly), else `BOT_CONFIG`,
    /// else [`DEFAULT_PATH`], then applies `BOT_*` env overrides, then
    /// validates.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("BOT_CONFIG").map(PathBuf::from));
        let file = match &explicit {
            Some(p) => {
                if !p.exists() {
                    bail!("config file {} not found", p.display());
                }
                Toml::file(p)
            }
            None => Toml::file(DEFAULT_PATH),
        };
        Self::from_figment(Figment::new().merge(file))
    }

    fn from_figment(f: Figment) -> anyhow::Result<Self> {
        let cfg: Self = f
            .merge(Env::prefixed("BOT_").ignore(&["CONFIG"]).split("__"))
            .extract()
            .context("invalid configuration")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(level) = &self.log.level {
            tracing_subscriber::EnvFilter::try_new(level)
                .with_context(|| format!("log.level {level:?} is not a valid filter"))?;
        }
        if self.whitelist.is_empty() {
            bail!("whitelist is empty: list the instruments the bot may trade");
        }
        if self.engine.history_len == 0 {
            bail!("engine.history_len must be >= 1");
        }
        if self.exchange.http_timeout_secs == 0 {
            bail!("exchange.http_timeout_secs must be >= 1");
        }
        if !self.api.listen.ip().is_loopback() && !self.api.allow_public {
            bail!(
                "api.listen {} is not loopback; set api.allow_public = true to expose it \
                 deliberately (and {API_TOKEN_VAR})",
                self.api.listen
            );
        }
        let venue = self.exchange.venue.id();
        if let Some(other) = self.whitelist.iter().find(|i| *i.venue() != venue) {
            bail!("whitelist has {other}, but exchange.venue is {venue}");
        }
        if self.exchange.venue == Venue::BinanceUsdm {
            if self.mode == Mode::Live {
                bail!(
                    "live perpetual orders are not supported yet; binance-usdm runs paper only \
                     (live futures prices, local fills and margin)"
                );
            }
            if self.paper.is_none() {
                bail!(
                    "exchange.venue = \"binance-usdm\" needs a [paper] section (initial_cash, fee, slippage_bps)"
                );
            }
            let Some(perp) = &self.perp else {
                bail!(
                    "exchange.venue = \"binance-usdm\" needs a [perp] section (leverage, margin)"
                );
            };
            if perp.leverage == 0 {
                bail!("perp.leverage must be >= 1");
            }
            if perp
                .clearance
                .is_some_and(|c| c < Decimal::ZERO || c >= Decimal::ONE)
            {
                bail!("perp.clearance must be in [0, 1)");
            }
            if self.exchange.interval == Interval::S1 {
                bail!("Binance futures serve no 1s klines; set exchange.interval to 1m or longer");
            }
            if self.exchange.rest_url.is_some() || self.exchange.ws_url.is_some() {
                bail!(
                    "exchange.rest_url and ws_url are spot overrides; remove them for binance-usdm"
                );
            }
        } else if self.perp.is_some() {
            bail!("[perp] is for exchange.venue = \"binance-usdm\"");
        }
        if self.exchange.venue == Venue::Max {
            if self.mode == Mode::Live {
                bail!("live orders on MAX are not supported yet; use mode = \"paper\"");
            }
            if self.paper.is_none() {
                bail!(
                    "exchange.venue = \"max\" needs a [paper] section (initial_cash, fee, slippage_bps)"
                );
            }
            if self.exchange.interval == Interval::S1 {
                bail!("MAX serves no 1s klines; set exchange.interval to 1m or longer");
            }
            if self.exchange.rest_url.is_some() || self.exchange.ws_url.is_some() {
                bail!("exchange.rest_url and ws_url are Binance overrides; remove them for MAX");
            }
        }
        if let Some(p) = &self.paper {
            if !p.initial_cash.is_positive() {
                bail!("paper.initial_cash must be positive");
            }
            if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
                bail!("paper.fee must be in [0, 1)");
            }
            if p.slippage_bps < Decimal::ZERO {
                bail!("paper.slippage_bps must be >= 0");
            }
        }
        if self.earn.enabled {
            let ratio = self
                .earn
                .ratio
                .context("earn.enabled needs earn.ratio (e.g. \"0.1\" for 10%)")?;
            if ratio <= Decimal::ZERO || ratio >= Decimal::ONE {
                bail!("earn.ratio must be in (0, 1)");
            }
            if self.earn.band < Decimal::ZERO || self.earn.band >= Decimal::new(5, 1) {
                bail!("earn.band must be in [0, 0.5)");
            }
            if self.earn.product == EarnProductKind::Locked && self.earn.locked_days.is_none() {
                bail!("earn.product = \"locked\" needs earn.locked_days");
            }
            if self.earn.interval_minutes == 0 {
                bail!("earn.interval_minutes must be >= 1");
            }
            if self.exchange.venue != Venue::Binance {
                bail!("earn (Simple Earn) is Binance only");
            }
            if self.exchange.network != Network::Live {
                bail!(
                    "earn needs exchange.network = \"live\": Binance serves Simple Earn on \
                     its live host only (demo and testnet answer 404)"
                );
            }
        }
        if !self.metrics.listen.ip().is_loopback() && !self.metrics.allow_public {
            bail!(
                "metrics.listen {} is not loopback; the endpoint has no authentication, \
                 so set metrics.allow_public = true to expose it deliberately",
                self.metrics.listen
            );
        }
        Ok(())
    }
}

#[cfg(test)]
// figment's Jail API returns its own large error type; not ours to shrink.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;

    const MIN: &str = r#"
        whitelist = ["binance:BTCUSDT"]
        [exchange]
        [strategy]
        name = "buy_and_hold"
        params = { coin = "binance:BTCUSDT", quote_amount = "100", fee = "0.001", slippage_bps = "5" }
    "#;

    fn parse(toml: &str) -> anyhow::Result<Config> {
        Config::from_figment(Figment::new().merge(Toml::string(toml)))
    }

    const MAX_PAPER: &str = r#"
        whitelist = ["max:BTCTWD"]
        [exchange]
        venue = "max"
        [paper]
        initial_cash = "30000"
        fee = "0.0015"
        slippage_bps = "20"
        [strategy]
        name = "dca"
        params = { coin = "max:BTCTWD", quote_amount = "1000", every = "day", fee = "0.0015", slippage_bps = "20" }
    "#;

    const USDM_PAPER: &str = r#"
        whitelist = ["binance-usdm:BTCUSDT"]
        [exchange]
        venue = "binance-usdm"
        interval = "1m"
        [paper]
        initial_cash = "1000"
        fee = "0.0005"
        slippage_bps = "2"
        [perp]
        leverage = 3
        margin = "cross"
        [strategy]
        name = "buy_and_hold"
        params = { coin = "binance-usdm:BTCUSDT", quote_amount = "100", fee = "0.0005", slippage_bps = "2" }
    "#;

    /// Perpetuals run paper only: `[paper]` and `[perp]` required, live
    /// refused at load, `[perp]` refused on another venue.
    #[test]
    fn perpetual_runs_are_paper_only() {
        Jail::expect_with(|_| {
            let c = parse(USDM_PAPER).unwrap();
            let p = c.perp.unwrap();
            assert_eq!(p.leverage, 3);
            assert_eq!(p.margin, bot_core::margin::MarginMode::Cross);
            assert_eq!((p.brackets_from, p.clearance), (Network::Demo, None));
            let live = format!("mode = \"live\"\n{USDM_PAPER}");
            assert!(
                parse(&live)
                    .unwrap_err()
                    .to_string()
                    .contains("not supported")
            );
            let no_perp = USDM_PAPER.replace(
                "[perp]\n        leverage = 3\n        margin = \"cross\"",
                "",
            );
            assert!(parse(&no_perp).unwrap_err().to_string().contains("[perp]"));
            let zero = USDM_PAPER.replace("leverage = 3", "leverage = 0");
            assert!(parse(&zero).is_err());
            let spot_with_perp = format!("{MIN}\n[perp]\nleverage = 2");
            assert!(
                parse(&spot_with_perp)
                    .unwrap_err()
                    .to_string()
                    .contains("[perp] is for")
            );
            Ok(())
        });
    }

    #[test]
    fn max_paper_loads() {
        Jail::expect_with(|_| {
            let c = parse(MAX_PAPER).unwrap();
            assert_eq!(c.exchange.venue, Venue::Max);
            let p = c.paper.unwrap();
            assert_eq!(p.initial_cash, Notional::new(Decimal::from(30_000)));
            assert_eq!(p.fee, Decimal::new(15, 4));
            assert_eq!(p.slippage_bps, Decimal::from(20));
            // Binance stays the default venue.
            assert_eq!(parse(MIN).unwrap().exchange.venue, Venue::Binance);
            Ok(())
        });
    }

    #[test]
    fn max_refuses_what_it_cannot_do() {
        Jail::expect_with(|_| {
            let live = MAX_PAPER.replace("whitelist", "mode = \"live\"\nwhitelist");
            assert!(
                parse(&live)
                    .unwrap_err()
                    .to_string()
                    .contains("not supported")
            );
            let no_paper = MAX_PAPER.replace("[paper]", "[unused]");
            assert!(parse(&no_paper).is_err());
            let start = MAX_PAPER.find("[paper]").unwrap();
            let end = MAX_PAPER.find("[strategy]").unwrap();
            let without = format!("{}{}", &MAX_PAPER[..start], &MAX_PAPER[end..]);
            let err = parse(&without).unwrap_err().to_string();
            assert!(err.contains("[paper]"), "{err}");
            let one_sec =
                MAX_PAPER.replace("venue = \"max\"", "venue = \"max\"\ninterval = \"1s\"");
            assert!(parse(&one_sec).is_err());
            let float = MAX_PAPER.replace("slippage_bps = \"20\"\n", "slippage_bps = 20\n");
            assert!(parse(&float).is_err(), "money as a bare number");
            let override_url = MAX_PAPER.replace(
                "venue = \"max\"",
                "venue = \"max\"\nrest_url = \"https://example.com\"",
            );
            assert!(parse(&override_url).is_err());
            Ok(())
        });
    }

    #[test]
    fn whitelist_must_be_on_the_venue() {
        Jail::expect_with(|_| {
            let mixed =
                MAX_PAPER.replace(r#"["max:BTCTWD"]"#, r#"["max:BTCTWD", "binance:BTCUSDT"]"#);
            let err = parse(&mixed).unwrap_err().to_string();
            assert!(err.contains("binance:BTCUSDT"), "{err}");
            let on_binance = MIN.replace(r#"["binance:BTCUSDT"]"#, r#"["max:BTCUSDT"]"#);
            assert!(parse(&on_binance).is_err());
            Ok(())
        });
    }

    #[test]
    fn defaults_are_safe() {
        Jail::expect_with(|_| {
            let c = parse(MIN).unwrap();
            assert_eq!(c.mode, Mode::Paper);
            assert_eq!(c.exchange.network, Network::Testnet);
            assert_eq!(c.exchange.interval, Interval::M1);
            assert_eq!(c.engine.history_len, 500);
            assert_eq!(
                c.engine.delist_exit(),
                std::time::Duration::from_secs(24 * 3600)
            );
            assert_eq!(c.exchange.http_timeout_secs, 10);
            assert_eq!(c.exchange.max_outage_secs, 600);
            assert!(c.exchange.user_stream);
            assert_eq!(c.strategy.params["quote_amount"].as_str(), Some("100"));
            Ok(())
        });
    }

    #[test]
    fn log_level_loads_and_bad_filters_fail() {
        Jail::expect_with(|_| {
            assert_eq!(parse(MIN).unwrap().log.level, None);
            let c = parse(&format!("{MIN}\n[log]\nlevel = \"debug,sqlx=error\"")).unwrap();
            assert_eq!(c.log.level.as_deref(), Some("debug,sqlx=error"));
            let err = parse(&format!("{MIN}\n[log]\nlevel = \"info,sqlx=loud\"")).unwrap_err();
            assert!(format!("{err:#}").contains("log.level"), "{err:#}");
            Ok(())
        });
    }

    #[test]
    fn log_level_from_env() {
        Jail::expect_with(|j| {
            j.set_env("BOT_LOG__LEVEL", "warn");
            assert_eq!(parse(MIN).unwrap().log.level.as_deref(), Some("warn"));
            Ok(())
        });
    }

    #[test]
    fn log_format_and_file_load() {
        Jail::expect_with(|j| {
            let c = parse(MIN).unwrap();
            assert_eq!((c.log.format, c.log.file), (LogFormat::Text, None));
            let c = parse(&format!(
                "{MIN}\n[log]\nformat = \"json\"\nfile = \"data/logs/run.jsonl\""
            ))
            .unwrap();
            assert_eq!(c.log.format, LogFormat::Json);
            assert_eq!(c.log.file, Some(PathBuf::from("data/logs/run.jsonl")));
            let err = parse(&format!("{MIN}\n[log]\nformat = \"xml\"")).unwrap_err();
            assert!(format!("{err:#}").contains("xml"), "{err:#}");
            j.set_env("BOT_LOG__FORMAT", "json");
            assert_eq!(parse(MIN).unwrap().log.format, LogFormat::Json);
            Ok(())
        });
    }

    /// RUST_LOG wins, then the config, then the default; an invalid
    /// RUST_LOG is skipped rather than half-applied.
    #[test]
    fn log_filter_precedence() {
        let d = "info,sqlx=warn";
        assert_eq!(log_filter(None, None, d), d);
        assert_eq!(log_filter(None, Some("debug"), d), "debug");
        assert_eq!(log_filter(Some("trace".into()), Some("debug"), d), "trace");
        assert_eq!(log_filter(Some("x=loud".into()), Some("debug"), d), "debug");
    }

    /// The reported error: `interval = "1s"` must load.
    #[test]
    fn one_second_interval_loads() {
        Jail::expect_with(|_| {
            let c = parse(&MIN.replace("[exchange]", "[exchange]\ninterval = \"1s\"")).unwrap();
            assert_eq!(c.exchange.interval, Interval::S1);
            Ok(())
        });
    }

    /// Off by default. Enabled: needs the live network, a ratio in (0, 1),
    /// a term for a locked product; then it maps to the sweep's settings.
    #[test]
    fn earn_is_off_by_default_and_live_only() {
        Jail::expect_with(|_| {
            assert!(parse(MIN).unwrap().earn.to_exchange().is_none());
            let with = |net: &str, earn: &str| {
                parse(&format!(
                    "{}\n[earn]\n{earn}",
                    MIN.replace("[exchange]", &format!("[exchange]\nnetwork = \"{net}\""))
                ))
            };
            let err = |r: anyhow::Result<Config>| r.unwrap_err().to_string();
            assert!(err(with("demo", "enabled = true\nratio = \"0.1\"")).contains("live"));
            assert!(err(with("live", "enabled = true")).contains("ratio"));
            assert!(err(with("live", "enabled = true\nratio = \"1\"")).contains("(0, 1)"));
            assert!(
                with("live", "enabled = true\nratio = 0.1").is_err(),
                "float"
            );
            assert!(
                err(with(
                    "live",
                    "enabled = true\nratio = \"0.1\"\nproduct = \"locked\""
                ))
                .contains("locked_days")
            );
            let c = with(
                "live",
                "enabled = true\nratio = \"0.1\"\nproduct = \"locked\"\nlocked_days = 30",
            )
            .unwrap();
            let e = c.earn.to_exchange().unwrap();
            assert_eq!(e.ratio, Decimal::new(1, 1));
            assert_eq!(e.band, Decimal::new(1, 2));
            assert_eq!(
                e.product,
                exchange::binance::earn::EarnProduct::Locked { days: 30 }
            );
            assert_eq!(e.every, std::time::Duration::from_secs(3600));
            Ok(())
        });
    }

    #[test]
    fn metrics_are_off_and_loopback_by_default() {
        Jail::expect_with(|_| {
            let c = parse(MIN).unwrap();
            assert!(!c.metrics.enabled && !c.metrics.include_money);
            assert_eq!(c.metrics.listen.to_string(), "127.0.0.1:9184");
            Ok(())
        });
    }

    #[test]
    fn api_is_off_and_loopback_by_default() {
        Jail::expect_with(|_| {
            let c = parse(MIN).unwrap();
            assert!(!c.api.enabled);
            assert_eq!(c.api.listen.to_string(), "127.0.0.1:9185");
            Ok(())
        });
    }

    #[test]
    fn public_api_needs_consent_and_a_token() {
        Jail::expect_with(|_| {
            let public = format!("{MIN}\n[api]\nenabled = true\nlisten = \"0.0.0.0:9185\"\n");
            assert!(parse(&public).is_err(), "no allow_public");
            let c = parse(&format!("{public}allow_public = true\n")).unwrap();
            assert!(api_token(&c.api, None).is_err(), "no token");
            assert!(api_token(&c.api, Some("  ".into())).is_err(), "blank token");
            assert_eq!(
                api_token(&c.api, Some("t".into())).unwrap().as_deref(),
                Some("t")
            );
            // Loopback: optional, enforced if set.
            let local = ApiSection::default();
            assert_eq!(api_token(&local, None).unwrap(), None);
            assert_eq!(
                api_token(&local, Some("t".into())).unwrap().as_deref(),
                Some("t")
            );
            Ok(())
        });
    }

    /// The token's env var must not collide with the BOT_ config layer.
    #[test]
    fn api_token_var_is_outside_the_config_prefix() {
        Jail::expect_with(|jail| {
            jail.set_env(API_TOKEN_VAR, "t");
            assert!(parse(MIN).is_ok());
            Ok(())
        });
    }

    #[test]
    fn public_metrics_need_explicit_consent() {
        Jail::expect_with(|_| {
            let public = format!("{MIN}\n[metrics]\nenabled = true\nlisten = \"0.0.0.0:9184\"\n");
            assert!(parse(&public).is_err());
            let consented = format!("{public}allow_public = true\n");
            assert!(parse(&consented).is_ok());
            Ok(())
        });
    }

    #[test]
    fn each_network_gets_its_own_default_database() {
        let d = DbSection::default();
        assert_eq!(
            d.url_for(Network::Testnet),
            "sqlite://data/testnet.db?mode=rwc"
        );
        assert_eq!(d.url_for(Network::Demo), "sqlite://data/demo.db?mode=rwc");
        assert_eq!(d.url_for(Network::Live), "sqlite://data/live.db?mode=rwc");
        let set = DbSection {
            url: Some("mysql://bot@db/bot".into()),
        };
        assert_eq!(set.url_for(Network::Live), "mysql://bot@db/bot");
    }

    #[test]
    fn secret_in_file_is_rejected() {
        Jail::expect_with(|_| {
            let with_key = MIN.replace("[exchange]", "[exchange]\napi_key = \"abc\"");
            assert!(parse(&with_key).is_err());
            Ok(())
        });
    }

    #[test]
    fn typos_and_empty_whitelist_fail() {
        Jail::expect_with(|_| {
            assert!(parse(&MIN.replace("[exchange]", "[exchange]\nnetwrok = \"demo\"")).is_err());
            // The old boolean is gone; a stale config must not load.
            assert!(parse(&MIN.replace("[exchange]", "[exchange]\ntestnet = true")).is_err());
            assert!(
                parse(&MIN.replace("[exchange]", "[exchange]\nnetwork = \"mainnet\"")).is_err()
            );
            assert!(parse(&MIN.replace(r#"["binance:BTCUSDT"]"#, "[]")).is_err());
            assert!(
                parse(&MIN.replace("[exchange]", "[exchange]\nhttp_timeout_secs = 0")).is_err()
            );
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        Jail::expect_with(|jail| {
            jail.set_env("BOT_EXCHANGE__NETWORK", "live");
            jail.set_env("BOT_MODE", "live");
            jail.set_env("BOT_CONFIG", "ignored.toml");
            jail.set_env("BOT_EXCHANGE__HTTP_TIMEOUT_SECS", "30");
            let c = parse(MIN).unwrap();
            assert_eq!(c.exchange.http_timeout_secs, 30);
            assert_eq!(c.exchange.network, Network::Live);
            assert_eq!(c.mode, Mode::Live);
            Ok(())
        });
    }

    #[test]
    fn explicit_missing_file_errors() {
        assert!(Config::load(Some(Path::new("/nonexistent/config.toml"))).is_err());
    }

    /// Every file in `config/examples/` is a valid config whose strategy
    /// builds from its params, and every registered strategy has one, so
    /// the examples can't rot or fall behind the registry. An example for a
    /// strategy behind a feature this build lacks is skipped; `make check`
    /// builds with the features on, so it is still tested.
    #[test]
    fn strategy_examples_load_and_cover_every_strategy() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/examples");
        let mut covered = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // In a jail, so no `BOT_*` variable in the shell leaks in.
            let mut loaded = None;
            Jail::expect_with(|_| {
                loaded = Some(parse(&text).unwrap_or_else(|e| panic!("{}: {e:#}", path.display())));
                Ok(())
            });
            let mut c = loaded.unwrap();
            if !strategy::names().contains(&c.strategy.name.as_str()) {
                // Behind a cargo feature this build lacks (`rhai`).
                continue;
            }
            // A script path is relative to the working directory, which
            // for `run` is the repo root.
            if let Some(toml::Value::String(script)) = c.strategy.params.get_mut("script") {
                *script = dir.join("../..").join(&*script).display().to_string();
            }
            let params = strategy::Params::new(c.strategy.params);
            strategy::build(&c.strategy.name, &params)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                path.file_stem().unwrap().to_str().unwrap(),
                c.strategy.name,
                "an example is named after its strategy"
            );
            covered.push(c.strategy.name);
        }
        covered.sort();
        let mut all: Vec<String> = strategy::names().into_iter().map(str::to_owned).collect();
        all.sort();
        assert_eq!(covered, all, "one example per registered strategy");
    }

    #[test]
    fn example_config_loads() {
        Jail::expect_with(|_| {
            let text = include_str!("../../../config/config.example.toml");
            parse(text).unwrap();
            Ok(())
        });
    }
}
