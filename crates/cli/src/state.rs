//! Saving and restoring the strategy's state for `run`.
//!
//! State is saved under the strategy's name with a hash of its params:
//! state saved for other params (another coin list, another period) doesn't
//! apply and is ignored. The engine's observer only publishes the latest
//! state on a `watch` channel; a background task writes it, so the trading
//! loop never waits for the database, and quick successive changes collapse
//! into one write. A failed write is a warning: it costs restart state, not
//! money.

use anyhow::Context as _;
use bot_core::Timestamp;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use storage::{SavedState, StrategyStateRepository};
use tokio::sync::watch;

/// SHA-256 over the strategy's name and its params table (whose keys
/// serialize in sorted order), in hex.
pub fn params_hash(name: &str, params: &toml::Table) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update([0]);
    h.update(params.to_string().as_bytes());
    hex::encode(h.finalize())
}

/// The state to resume from: `strategy`'s saved state, if it was saved
/// under the same params.
pub async fn load(
    books: &dyn StrategyStateRepository,
    strategy: &str,
    hash: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(saved) = books
        .load_state(strategy)
        .await
        .context("loading strategy state")?
    else {
        return Ok(None);
    };
    if saved.params_hash != hash {
        tracing::warn!(strategy, "saved state is for other params; starting fresh");
        return Ok(None);
    }
    match serde_json::from_str(&saved.state) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            tracing::warn!(strategy, error = %e, "saved state is not valid JSON; starting fresh");
            Ok(None)
        }
    }
}

/// Writes the latest published state in the background.
pub struct Saver {
    tx: watch::Sender<Option<serde_json::Value>>,
    task: tokio::task::JoinHandle<()>,
}

impl Saver {
    /// Starts the writer; `initial` is what is already stored, so it isn't
    /// written again.
    pub fn start(
        books: Arc<dyn StrategyStateRepository>,
        strategy: String,
        hash: String,
        initial: Option<serde_json::Value>,
        now: fn() -> Timestamp,
    ) -> Self {
        let (tx, mut rx) = watch::channel(initial);
        rx.mark_unchanged();
        let task = tokio::spawn(async move {
            // Ends once the sender is dropped and the last value is written.
            while rx.changed().await.is_ok() {
                let Some(state) = rx.borrow_and_update().clone() else {
                    continue;
                };
                let saved = SavedState {
                    params_hash: hash.clone(),
                    state: state.to_string(),
                    updated_at: now(),
                };
                if let Err(e) = books.save_state(&strategy, &saved).await {
                    tracing::warn!(error = %e, "strategy state not saved; a restart would start fresh");
                }
            }
        });
        Self { tx, task }
    }

    /// Publishes `state` if it differs from the last one.
    pub fn publish(&self, state: &Option<serde_json::Value>) {
        if state.is_some() {
            self.tx.send_if_modified(|cur| {
                if cur == state {
                    false
                } else {
                    cur.clone_from(state);
                    true
                }
            });
        }
    }

    /// Waits until the last published state is written.
    pub async fn finish(self) {
        drop(self.tx);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::SqlStore;

    #[test]
    fn hash_depends_on_name_and_params_only() {
        let a: toml::Table = toml::from_str("coins = [\"binance:BTCUSDT\"]\nlookback = 2").unwrap();
        let reordered: toml::Table =
            toml::from_str("lookback = 2\ncoins = [\"binance:BTCUSDT\"]").unwrap();
        let other: toml::Table =
            toml::from_str("coins = [\"binance:BTCUSDT\"]\nlookback = 3").unwrap();
        assert_eq!(params_hash("m", &a), params_hash("m", &reordered));
        assert_ne!(params_hash("m", &a), params_hash("m", &other));
        assert_ne!(params_hash("m", &a), params_hash("n", &a));
    }

    struct Db(Arc<SqlStore>, std::path::PathBuf);
    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.1);
        }
    }
    async fn db() -> Db {
        let path = std::env::temp_dir().join(format!("trade-bot-state-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = SqlStore::open(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        Db(Arc::new(s), path)
    }

    fn at() -> Timestamp {
        Timestamp::from_millis(42)
    }

    /// The last state published before finish is the one stored, it loads
    /// back under the same params, and not under others.
    #[tokio::test]
    async fn latest_state_is_written_and_loads_back() {
        let db = db().await;
        let saver = Saver::start(db.0.clone(), "s".into(), "h1".into(), None, at);
        saver.publish(&Some(serde_json::json!({ "v": 1, "n": 1 })));
        saver.publish(&Some(serde_json::json!({ "v": 1, "n": 2 })));
        saver.publish(&None);
        saver.finish().await;
        assert_eq!(
            load(&*db.0, "s", "h1").await.unwrap(),
            Some(serde_json::json!({ "v": 1, "n": 2 }))
        );
        assert_eq!(load(&*db.0, "s", "other").await.unwrap(), None);
        assert_eq!(load(&*db.0, "nobody", "h1").await.unwrap(), None);
    }
}
