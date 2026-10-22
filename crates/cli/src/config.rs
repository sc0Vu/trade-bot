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
        if self.whitelist.is_empty() {
            bail!("whitelist is empty: list the instruments the bot may trade");
        }
        if self.engine.history_len == 0 {
            bail!("engine.history_len must be >= 1");
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
        params = { instrument = "binance:BTCUSDT", quote_amount = "100" }
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
            assert_eq!(c.strategy.params["quote_amount"].as_str(), Some("100"));
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
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file() {
        Jail::expect_with(|jail| {
            jail.set_env("BOT_EXCHANGE__NETWORK", "live");
            jail.set_env("BOT_MODE", "live");
            jail.set_env("BOT_CONFIG", "ignored.toml");
            let c = parse(MIN).unwrap();
            assert_eq!(c.exchange.network, Network::Live);
            assert_eq!(c.mode, Mode::Live);
            Ok(())
        });
    }

    #[test]
    fn explicit_missing_file_errors() {
        assert!(Config::load(Some(Path::new("/nonexistent/config.toml"))).is_err());
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
