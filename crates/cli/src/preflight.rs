//! What a `run` decides from its config and flags before it connects to
//! anything: which kind of run it is (venue, network, local or venue
//! fills), where its books live, and where its kill-switch marker is.
//!
//! One function for both `run` and `config check`, so a check can't pass
//! a config that `run` would refuse for a reason decided here.

use crate::config::{self, Config, PerpFills, Venue};
use crate::gate;
use exchange::binance::Network;
use std::path::PathBuf;

/// The kind of run a config and its flags select. Every variant with a
/// network has passed the live gate (invariant 7) for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Binance spot orders on the network.
    Spot(Network),
    /// Binance USDⓈ-M orders on the network's futures host
    /// (`perp.fills = "venue"`).
    UsdmOrders(Network),
    /// Binance USDⓈ-M prices, local paper fills and margin
    /// (`perp.fills = "local"`).
    UsdmPaper,
    /// eToro: the demo account on a fake-money network, the real one on
    /// live.
    Etoro(Network),
    /// MAX prices, local paper fills (MAX has no testnet).
    MaxPaper,
}

impl Route {
    /// Where this run keeps its books: `db.url` if set, else its default
    /// SQLite file under `data/`. `None` for the local paper runs, whose
    /// books are in memory (every run starts flat).
    pub fn books_url(self, db: &config::DbSection) -> Option<String> {
        let default = |suffix: &str, n: Network| {
            db.url.clone().unwrap_or_else(|| {
                format!(
                    "sqlite://data/{}{suffix}.db?mode=rwc",
                    config::network_name(n)
                )
            })
        };
        match self {
            Self::Spot(n) => Some(db.url_for(n)),
            Self::UsdmOrders(n) => Some(default("-usdm", n)),
            Self::Etoro(n) => Some(default("-etoro", n)),
            Self::UsdmPaper | Self::MaxPaper => None,
        }
    }

    /// True when orders reach a real-money account.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Spot(Network::Live)
                | Self::UsdmOrders(Network::Live)
                | Self::Etoro(Network::Live)
        )
    }

    /// What the run does, in a few words.
    pub fn describe(self) -> String {
        let n = config::network_name;
        match self {
            Self::Spot(net) => format!("orders on Binance spot {}", n(net)),
            Self::UsdmOrders(net) => format!("orders on Binance USDⓈ-M futures {}", n(net)),
            Self::UsdmPaper => {
                "Binance USDⓈ-M prices, local paper fills; nothing sent to Binance".into()
            }
            Self::Etoro(Network::Live) => "orders on the real eToro account".into(),
            Self::Etoro(_) => "orders on the eToro demo account".into(),
            Self::MaxPaper => "MAX prices, local paper fills; nothing sent to MAX".into(),
        }
    }
}

/// Decides the run from `cfg` and the `run` flags, or refuses: a run that
/// can't be built, a gate lock missing (any venue), a Binance URL
/// override pointing a fake-money run at mainnet.
pub fn route(cfg: &Config, live: bool, i_understand: bool) -> anyhow::Result<Route> {
    // Spot and futures in one run aren't built: started, it would see one
    // leg's instrument and quietly do nothing.
    anyhow::ensure!(
        cfg.strategy.name != "funding_carry",
        "funding_carry trades Binance spot and USDⓈ-M together; a run trades one venue, \
         so it is backtest only for now (backtest --perp-symbol)"
    );
    Ok(match cfg.exchange.venue {
        Venue::BinanceUsdm => {
            let venue_fills = cfg
                .perp
                .as_ref()
                .is_some_and(|p| p.fills == PerpFills::Venue);
            if venue_fills {
                // The same three locks as spot: --paper = testnet/demo only.
                Route::UsdmOrders(gate::decide(
                    cfg.mode,
                    cfg.exchange.network,
                    live,
                    i_understand,
                )?)
            } else {
                anyhow::ensure!(
                    !live,
                    "perp.fills = \"local\" is a paper run; set perp.fills = \"venue\" for orders on Binance"
                );
                Route::UsdmPaper
            }
        }
        // The same three locks: --paper = demo account only.
        Venue::Etoro => Route::Etoro(gate::decide(
            cfg.mode,
            cfg.exchange.network,
            live,
            i_understand,
        )?),
        Venue::Max => {
            gate::decide_max(cfg.mode, live)?;
            Route::MaxPaper
        }
        Venue::Binance => {
            let network = gate::decide(cfg.mode, cfg.exchange.network, live, i_understand)?;
            gate::check_overrides(
                network,
                &[
                    cfg.exchange.rest_url.as_deref(),
                    cfg.exchange.ws_url.as_deref(),
                ],
            )?;
            Route::Spot(network)
        }
    })
}

/// Where a tripped kill switch is remembered for this venue, network and
/// strategy (`data/kill-switch/<venue>-<network>-<strategy>.txt`).
pub fn kill_marker(cfg: &Config) -> PathBuf {
    PathBuf::from("data/kill-switch").join(format!(
        "{}-{}-{}.txt",
        cfg.exchange.venue.id().as_str(),
        config::network_name(cfg.exchange.network),
        cfg.strategy.name
    ))
}

#[cfg(test)]
// figment's Jail API returns its own large error type; not ours to shrink.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;

    /// `body` after a whitelist and strategy on `venue`, parsed in a jail
    /// so no `BOT_*` variable in the shell leaks in.
    fn cfg(head: &str, venue_symbol: &str, body: &str) -> Config {
        let text = format!(
            r#"{head}
            whitelist = ["{venue_symbol}"]
            {body}
            [strategy]
            name = "buy_and_hold"
            params = {{ coin = "{venue_symbol}", quote_amount = "100", fee = "0.001", slippage_bps = "5" }}
            "#
        );
        let mut out = None;
        Jail::expect_with(|_| {
            out = Some(Config::parse_str(&text).unwrap_or_else(|e| panic!("{e:#}\n{text}")));
            Ok(())
        });
        out.unwrap()
    }

    const PAPER: &str = r#"[paper]
            initial_cash = "1000"
            fee = "0.001"
            slippage_bps = "5""#;

    /// Each venue's config picks its run; the network is the configured
    /// one on paper and live only with every lock.
    #[test]
    fn each_venue_picks_its_run() {
        let spot = cfg("", "binance:BTCUSDT", "[exchange]\nnetwork = \"demo\"");
        assert_eq!(
            route(&spot, false, false).unwrap(),
            Route::Spot(Network::Demo)
        );
        let live = cfg(
            "mode = \"live\"",
            "binance:BTCUSDT",
            "[exchange]\nnetwork = \"live\"",
        );
        assert!(
            route(&live, false, false).is_err(),
            "live config, no --live"
        );
        assert!(route(&live, true, false).is_err(), "no --i-understand");
        assert_eq!(
            route(&live, true, true).unwrap(),
            Route::Spot(Network::Live)
        );

        let max = cfg(
            "",
            "max:BTCUSDT",
            &format!("[exchange]\nvenue = \"max\"\n{PAPER}"),
        );
        assert_eq!(route(&max, false, false).unwrap(), Route::MaxPaper);
        assert!(route(&max, true, true).is_err(), "MAX has no live orders");

        let etoro = cfg(
            "",
            "etoro:AAPL",
            "[exchange]\nvenue = \"etoro\"\ninterval = \"1d\"",
        );
        assert_eq!(
            route(&etoro, false, false).unwrap(),
            Route::Etoro(Network::Testnet)
        );

        let usdm = |fills: &str| {
            cfg(
                "",
                "binance-usdm:BTCUSDT",
                &format!(
                    "[exchange]\nvenue = \"binance-usdm\"\ninterval = \"1m\"\n{PAPER}\n\
                     [perp]\nleverage = 2\nmargin = \"isolated\"\nfills = \"{fills}\""
                ),
            )
        };
        assert_eq!(
            route(&usdm("local"), false, false).unwrap(),
            Route::UsdmPaper
        );
        let err = route(&usdm("local"), true, true).unwrap_err().to_string();
        assert!(err.contains("is a paper run"), "{err}");
        assert_eq!(
            route(&usdm("venue"), false, false).unwrap(),
            Route::UsdmOrders(Network::Testnet)
        );
    }

    /// funding_carry is backtest only; a fake-money spot run pointed at a
    /// mainnet host is refused before any key is read.
    #[test]
    fn refusals_before_connecting() {
        let mut carry = cfg("", "binance:BTCUSDT", "[exchange]");
        carry.strategy.name = "funding_carry".into();
        let err = route(&carry, false, false).unwrap_err().to_string();
        assert!(err.contains("backtest only"), "{err}");
        let pointed = cfg(
            "",
            "binance:BTCUSDT",
            "[exchange]\nrest_url = \"https://api.binance.com\"",
        );
        let err = route(&pointed, false, false).unwrap_err().to_string();
        assert!(err.contains("mainnet host"), "{err}");
    }

    /// Books: one file per network and venue family unless `db.url`; the
    /// local paper runs keep none on disk.
    #[test]
    fn books_live_where_each_run_opens_them() {
        let none = config::DbSection::default();
        let set = config::DbSection {
            url: Some("sqlite://x.db".into()),
        };
        assert_eq!(
            Route::Spot(Network::Testnet).books_url(&none).as_deref(),
            Some("sqlite://data/testnet.db?mode=rwc")
        );
        assert_eq!(
            Route::UsdmOrders(Network::Demo).books_url(&none).as_deref(),
            Some("sqlite://data/demo-usdm.db?mode=rwc")
        );
        assert_eq!(
            Route::Etoro(Network::Live).books_url(&none).as_deref(),
            Some("sqlite://data/live-etoro.db?mode=rwc")
        );
        assert_eq!(
            Route::Etoro(Network::Live).books_url(&set).as_deref(),
            Some("sqlite://x.db")
        );
        assert_eq!(Route::UsdmPaper.books_url(&set), None);
        assert_eq!(Route::MaxPaper.books_url(&none), None);
    }

    #[test]
    fn kill_marker_names_venue_network_strategy() {
        let c = cfg(
            "",
            "max:BTCUSDT",
            &format!("[exchange]\nvenue = \"max\"\n{PAPER}"),
        );
        assert_eq!(
            kill_marker(&c),
            PathBuf::from("data/kill-switch/max-testnet-buy_and_hold.txt")
        );
    }
}
