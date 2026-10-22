//! The live-trading gate (invariant 7). Three independent locks must all
//! be open before real orders reach mainnet: `mode = "live"` in config,
//! `exchange.testnet = false`, and `--live --i-understand` on the command
//! line. A mistake here spends real money, so this is a pure function
//! with every combination tested.

use crate::config::Mode;
use anyhow::bail;

/// Which network a run may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// Testnet: fake balances.
    Testnet,
    /// Mainnet: real money.
    Mainnet,
}

/// True if `url`'s host is a Binance mainnet host (`binance.com` or a
/// subdomain). Testnet lives on `binance.vision`.
fn is_mainnet_host(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "binance.com" || host.ends_with(".binance.com")
}

/// Refuses URL overrides that would send a testnet run to mainnet hosts,
/// which would bypass every lock below.
pub fn check_overrides(network: Network, urls: &[Option<&str>]) -> anyhow::Result<()> {
    if network == Network::Testnet
        && let Some(u) = urls.iter().flatten().find(|u| is_mainnet_host(u))
    {
        bail!("exchange URL override {u} is a mainnet host but this run is testnet");
    }
    Ok(())
}

/// Decides the network from config and flags, or refuses.
///
/// Without `--live`: testnet only. A config set to `mode = "live"` or
/// `testnet = false` is refused rather than quietly downgraded, so the
/// mismatch is noticed. With `--live`: every missing lock is listed.
pub fn decide(
    mode: Mode,
    testnet: bool,
    live_flag: bool,
    i_understand: bool,
) -> anyhow::Result<Network> {
    if !live_flag {
        if mode == Mode::Live {
            bail!("config says mode = \"live\" but --live was not given; refusing to guess");
        }
        if !testnet {
            bail!("paper mode runs only against testnet; set exchange.testnet = true");
        }
        return Ok(Network::Testnet);
    }
    let mut missing = Vec::new();
    if mode != Mode::Live {
        missing.push("mode = \"live\" in config");
    }
    if testnet {
        missing.push("exchange.testnet = false in config");
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
    Ok(Network::Mainnet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_only_with_all_three_locks() {
        for mode in [Mode::Paper, Mode::Live] {
            for testnet in [false, true] {
                for live in [false, true] {
                    for iu in [false, true] {
                        let got = decide(mode, testnet, live, iu).ok();
                        let all_locks = mode == Mode::Live && !testnet && live && iu;
                        if all_locks {
                            assert_eq!(got, Some(Network::Mainnet));
                        } else {
                            assert_ne!(
                                got,
                                Some(Network::Mainnet),
                                "{mode:?} testnet={testnet} live={live} iu={iu}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paper_is_testnet_and_never_downgrades_silently() {
        assert_eq!(
            decide(Mode::Paper, true, false, false).unwrap(),
            Network::Testnet
        );
        assert!(decide(Mode::Paper, false, false, false).is_err());
        assert!(decide(Mode::Live, true, false, false).is_err());
    }

    #[test]
    fn testnet_run_cannot_be_pointed_at_mainnet() {
        let t = Network::Testnet;
        assert!(check_overrides(t, &[Some("https://api.binance.com"), None]).is_err());
        assert!(check_overrides(t, &[None, Some("wss://stream.binance.com:9443")]).is_err());
        assert!(check_overrides(t, &[Some("https://API.BINANCE.COM/")]).is_err());
        assert!(
            check_overrides(
                t,
                &[
                    Some("https://testnet.binance.vision"),
                    Some("http://localhost:8080")
                ]
            )
            .is_ok()
        );
        assert!(check_overrides(t, &[Some("https://notbinance.com.evil")]).is_ok());
        assert!(check_overrides(Network::Mainnet, &[Some("https://api.binance.com")]).is_ok());
    }

    #[test]
    fn live_lists_every_missing_lock() {
        let msg = decide(Mode::Paper, true, true, false)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("mode") && msg.contains("testnet") && msg.contains("--i-understand"),
            "{msg}"
        );
    }
}
