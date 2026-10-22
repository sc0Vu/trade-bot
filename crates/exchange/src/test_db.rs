//! Test support: a throwaway migrated database for broker books.

use std::sync::Arc;
use storage::SqlStore;

/// A migrated SQLite file, deleted on drop. A file rather than
/// `:memory:`, where each pooled connection could see its own database.
pub struct TempDb {
    pub store: Arc<SqlStore>,
    path: std::path::PathBuf,
}

impl TempDb {
    pub async fn new() -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("trade-bot-broker-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Arc::new(SqlStore::open(&url).await.unwrap());
        Self { store, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
