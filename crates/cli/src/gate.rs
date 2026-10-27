//! The live-trading gate (invariant 7). Three independent locks must all
//! be open before real orders reach mainnet: `mode = "live"` in config,
//! `exchange.network = "live"`, and `--live --i-understand` on the command
//! line. A mistake here spends real money, so this is a pure function
//! with every combination tested.

use crate::config::Mode;
use anyhow::{Context, bail};
use exchange::binance::Network;

/// Binance hosts that trade fake balances although they sit under
/// `binance.com` (demo trading). Every other `binance.com` host is treated
/// as real money.
const DEMO_HOSTS: [&str; 2] = ["demo-api.binance.com", "demo-stream.binance.com"];

/// True if `url`'s host is a Binance real-money host: `binance.com` or a
/// subdomain, except the demo-trading hosts. Testnet lives on
/// `binance.vision`. Parsed as a URL, as the HTTP client will, so
/// credentials before the host, percent-encoding or a trailing dot can't
/// disguise it; an override that doesn't parse is an error.
fn is_mainnet_host(url: &str) -> anyhow::Result<bool> {
    let parsed = url::Url::parse(url)
        .with_context(|| format!("exchange URL override {url:?} is not a URL"))?;
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    Ok((host == "binance.com" || host.ends_with(".binance.com")) && !DEMO_HOSTS.contains(&&*host))
}

/// Refuses URL overrides that would send a fake-money run to mainnet
/// hosts, which would bypass every lock below.
pub fn check_overrides(network: Network, urls: &[Option<&str>]) -> anyhow::Result<()> {
    for u in urls.iter().flatten() {
        if is_mainnet_host(u)? && network.is_fake_money() {
            bail!("exchange URL override {u} is a mainnet host but this run is {network:?}");
        }
    }
    Ok(())
}

/// Decides the network from config and flags, or refuses.
///
/// Without `--live`: the configured fake-money network (testnet or demo).
/// A config set to `mode = "live"` or `network = "live"` is refused rather
/// than quietly downgraded, so the mismatch is noticed. With `--live`:
/// every missing lock is listed.
pub fn decide(
    mode: Mode,
    network: Network,
    live_flag: bool,
    i_understand: bool,
) -> anyhow::Result<Network> {
    if !live_flag {
        if mode == Mode::Live {
            bail!("config says mode = \"live\" but --live was not given; refusing to guess");
        }
        if !network.is_fake_money() {
            bail!(
                "paper mode runs only on fake money; set exchange.network = \"testnet\" or \"demo\""
            );
        }
        return Ok(network);
    }
    let mut missing = Vec::new();
    if mode != Mode::Live {
        missing.push("mode = \"live\" in config");
    }
    if network != Network::Live {
        missing.push("exchange.network = \"live\" in config");
    }
    if !i_understand {
        missing.push("--i-understand flag");
    }
    if !missing.is_empty() {
        bail!(
            "live trading needs all three locks; missing: {}",
            missing.join(", ")
        );
    }
    Ok(Network::Live)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETWORKS: [Network; 3] = [Network::Testnet, Network::Demo, Network::Live];

    #[test]
    fn mainnet_only_with_all_three_locks() {
        for mode in [Mode::Paper, Mode::Live] {
            for network in NETWORKS {
                for live in [false, true] {
                    for iu in [false, true] {
                        let got = decide(mode, network, live, iu).ok();
                        let all_locks =
                            mode == Mode::Live && network == Network::Live && live && iu;
                        if all_locks {
                            assert_eq!(got, Some(Network::Live));
                        } else {
                            assert_ne!(
                                got,
                                Some(Network::Live),
                                "{mode:?} {network:?} live={live} iu={iu}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paper_runs_the_configured_fake_network_and_never_downgrades() {
        assert_eq!(
            decide(Mode::Paper, Network::Testnet, false, false).unwrap(),
            Network::Testnet
        );
        assert_eq!(
            decide(Mode::Paper, Network::Demo, false, false).unwrap(),
            Network::Demo
        );
        assert!(decide(Mode::Paper, Network::Live, false, false).is_err());
        assert!(decide(Mode::Live, Network::Testnet, false, false).is_err());
    }

    #[test]
    fn fake_money_run_cannot_be_pointed_at_mainnet() {
        for n in [Network::Testnet, Network::Demo] {
            assert!(check_overrides(n, &[Some("https://api.binance.com"), None]).is_err());
            assert!(check_overrides(n, &[None, Some("wss://stream.binance.com:9443")]).is_err());
            assert!(check_overrides(n, &[Some("https://API.BINANCE.COM/")]).is_err());
            assert!(check_overrides(n, &[Some("https://api1.binance.com")]).is_err());
            // A lookalike of a demo host is still mainnet.
            assert!(
                check_overrides(n, &[Some("https://demo-api.binance.com.binance.com")]).is_err()
            );
            assert!(
                check_overrides(
                    n,
                    &[
                        Some("https://testnet.binance.vision"),
                        Some("http://localhost:8080")
                    ]
                )
                .is_ok()
            );
            assert!(
                check_overrides(
                    n,
                    &[
                        Some("https://demo-api.binance.com"),
                        Some("wss://demo-stream.binance.com:9443")
                    ]
                )
                .is_ok()
            );
            assert!(check_overrides(n, &[Some("https://notbinance.com.evil")]).is_ok());
        }
        assert!(check_overrides(Network::Live, &[Some("https://api.binance.com")]).is_ok());
    }

    /// Spellings that reach the same mainnet host: credentials before the
    /// host (`user:pass@`), the fully-qualified trailing dot, and a
    /// percent-encoded dot all connect to api.binance.com.
    #[test]
    fn mainnet_spelled_differently_is_still_mainnet() {
        for u in [
            "https://user:pass@api.binance.com",
            "https://key@api.binance.com/api",
            "https://api.binance.com.",
            "https://api.binance.com.:443/",
            "wss://stream.binance.com.:9443",
            "https://api%2Ebinance.com",
        ] {
            assert!(
                check_overrides(Network::Testnet, &[Some(u)]).is_err(),
                "{u}"
            );
        }
        assert!(check_overrides(Network::Demo, &[Some("https://demo-api.binance.com.")]).is_ok());
    }

    /// No scheme means no host to check: refused at startup, not guessed.
    #[test]
    fn override_that_is_not_a_url_is_refused() {
        let err = check_overrides(Network::Testnet, &[Some("api.binance.com")]).unwrap_err();
        assert!(format!("{err:#}").contains("not a URL"), "{err:#}");
    }

    #[test]
    fn live_lists_every_missing_lock() {
        let msg = decide(Mode::Paper, Network::Demo, true, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("mode") && msg.contains("network") && msg.contains("--i-understand"),
            "{msg}"
        );
    }
}
