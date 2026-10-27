//! Run configuration: `config/config.toml` (or `--config` / `BOT_CONFIG`),
//! then `BOT_*` environment variables (`BOT_EXCHANGE__NETWORK=demo`),
//! into one typed struct.
//!
//! `deny_unknown_fields` everywhere: a typo is an error, never a silent
//! default, and there is no field a secret could go in, so `api_key = ...`
//! in a file fails to load (invariant 6). Keys come only from
//! `BINANCE_API_KEY` / `BINANCE_API_SECRET`.

use anyhow::{Context, bail};
use bot_core::{InstrumentId, Interval};
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
    /// `testnet` (default), `demo` or `live`. Keys must belong to it.
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
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            history_len: five_hundred(),
        }
    }
}

fn one_minute() -> Interval {
    Interval::M1
}
fn five_hundred() -> usize {
    500
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

    #[test]
    fn defaults_are_safe() {
        Jail::expect_with(|_| {
            let c = parse(MIN).unwrap();
            assert_eq!(c.mode, Mode::Paper);
            assert_eq!(c.exchange.network, Network::Testnet);
            assert_eq!(c.exchange.interval, Interval::M1);
            assert_eq!(c.engine.history_len, 500);
            assert_eq!(c.exchange.http_timeout_secs, 10);
            assert_eq!(c.exchange.max_outage_secs, 600);
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
    /// the examples can't rot or fall behind the registry.
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
            let c = loaded.unwrap();
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
