//! Strategies written as scripts, loaded at startup instead of compiled in.
//!
//! A script strategy is an adapter: one Rust type that implements
//! [`Strategy`](crate::Strategy) and forwards each hook into the script. The
//! engine, risk, brokers and backtester see an ordinary strategy, so a
//! script runs unchanged in backtest and live (invariant 3) and reaches the
//! world only through a snapshot of [`Context`](crate::Context)
//! (invariant 5).
//!
//! This module holds what every runtime shares: where the script comes
//! from (`strategy.params.script`), what the rest of the params are, and
//! the envelope saved state travels in. Each runtime (today only
//! [`rhai`]) binds the same API to its language.
//!
//! Every runtime must keep:
//! - money exact (invariant 1): no float reaches a script;
//! - no file, network, clock or randomness: a script decides from its
//!   inputs alone, the same in backtest and live;
//! - a budget per hook call, so a runaway script can't hang the bot;
//! - errors loud: a script error is a [`Strategy::fault`](crate::Strategy::fault)
//!   and stops the run, never a silently skipped decision.

pub mod rhai;

use crate::StrategyError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Format version of the saved-state envelope.
const ENVELOPE_VERSION: u64 = 1;

/// A loaded script: its text, and a digest so state saved by one version
/// of a script is never handed to another.
pub(crate) struct Source {
    pub path: PathBuf,
    pub text: String,
    /// Hex SHA-256 of `text`.
    pub digest: String,
}

/// Splits `strategy.params` into the script path (`script`, required) and
/// the rest, which is the script's own params.
pub(crate) fn split_params(
    name: &str,
    params: &crate::Params,
) -> Result<(PathBuf, toml::Table), StrategyError> {
    let mut table: toml::Table = params.parse(name)?;
    let bad = |reason: &str| StrategyError::Params {
        strategy: name.to_owned(),
        reason: reason.to_owned(),
    };
    match table.remove("script") {
        Some(toml::Value::String(path)) if !path.is_empty() => Ok((PathBuf::from(path), table)),
        Some(_) => Err(bad("`script` must be a non-empty path string")),
        None => Err(bad("missing `script`: the path of the script to run")),
    }
}

/// Reads the script at `path`, relative to the working directory.
pub(crate) fn load(name: &str, path: &Path) -> Result<Source, StrategyError> {
    let text = std::fs::read_to_string(path).map_err(|e| StrategyError::Params {
        strategy: name.to_owned(),
        reason: format!("can't read script {}: {e}", path.display()),
    })?;
    let digest = hex::encode(Sha256::digest(text.as_bytes()));
    Ok(Source {
        path: path.to_owned(),
        text,
        digest,
    })
}

/// Wraps a script's own state for saving, stamped with the script digest.
pub(crate) fn wrap_state(digest: &str, state: Value) -> Value {
    json!({ "v": ENVELOPE_VERSION, "script_sha256": digest, "state": state })
}

/// The script's own state from a saved envelope, if it was saved by this
/// very script (same digest) in this envelope format. State from an edited
/// script is refused: its meaning may have changed.
pub(crate) fn unwrap_state(name: &str, digest: &str, saved: Value) -> Result<Value, StrategyError> {
    let refuse = |reason: String| StrategyError::State {
        strategy: name.to_owned(),
        reason,
    };
    if saved["v"].as_u64() != Some(ENVELOPE_VERSION) {
        return Err(refuse(format!(
            "format version {:?}, expected {ENVELOPE_VERSION}",
            saved["v"].as_u64()
        )));
    }
    if saved["script_sha256"].as_str() != Some(digest) {
        return Err(refuse("saved by another version of the script".to_owned()));
    }
    let Value::Object(mut map) = saved else {
        return Err(refuse("not an object".to_owned()));
    };
    Ok(map.remove("state").unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Params;

    #[test]
    fn script_path_is_split_from_the_scripts_params() {
        let p = Params::from_toml("script = \"a.rhai\"\nfee = \"0.001\"").unwrap();
        let (path, rest) = split_params("rhai", &p).unwrap();
        assert_eq!(path, PathBuf::from("a.rhai"));
        assert_eq!(rest.len(), 1);
        assert_eq!(rest["fee"].as_str(), Some("0.001"));
        for bad in ["fee = \"1\"", "script = 3", "script = \"\""] {
            let p = Params::from_toml(bad).unwrap();
            assert!(split_params("rhai", &p).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_script_file_is_a_params_error() {
        let e = load("rhai", Path::new("/no/such/script.rhai"))
            .err()
            .unwrap();
        assert!(matches!(e, StrategyError::Params { .. }), "{e}");
    }

    #[test]
    fn state_round_trips_only_for_the_same_script() {
        let saved = wrap_state("abc", json!({"n": 1}));
        assert_eq!(
            unwrap_state("rhai", "abc", saved.clone()).unwrap(),
            json!({"n": 1})
        );
        assert!(unwrap_state("rhai", "def", saved).is_err());
        let other_format = json!({"v": 2, "script_sha256": "abc", "state": {}});
        assert!(unwrap_state("rhai", "abc", other_format).is_err());
    }
}
