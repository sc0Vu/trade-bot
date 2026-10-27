//! Log output: the console, as text or JSON, and for `run` an optional
//! JSON-lines file for a log shipper (Grafana Alloy into Loki, Vector, the
//! Datadog agent).
//!
//! A JSON line is one flat object: `timestamp` (RFC 3339, UTC), `level`
//! (`INFO`, `WARN`, …), `target` (module path), `message`, then the
//! event's own fields, and inside `run` a `span` object whose `tag` is the
//! run's tag. Engine order ids restart at `o-1` every run; the tag (also
//! the prefix of Binance's `newClientOrderId`) tells runs apart.
//! `deploy/alloy/config.alloy` reads exactly these keys; keep them in step. Logging can never stop trading: a file that
//! can't be opened is a warning on the console, and the run goes on.

use crate::config::{LogFormat, LogSection};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

type Boxed = Box<dyn Layer<Registry> + Send + Sync>;

/// A JSON-lines layer writing to `w`, in the shape the module doc states.
fn json<W>(w: W) -> Boxed
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_span_list(false)
        .with_ansi(false)
        .with_writer(w)
        .boxed()
}

/// Opens `path` for appending, creating it and its directory.
fn open(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Installs the global subscriber: `filter` (already validated) for every
/// output, the console in `cfg.format`, and JSON lines appended to
/// `cfg.file` if set.
pub fn init(filter: &str, cfg: &LogSection) {
    let mut layers: Vec<Boxed> = vec![match cfg.format {
        LogFormat::Text => tracing_subscriber::fmt::layer().boxed(),
        LogFormat::Json => json(std::io::stdout),
    }];
    let mut failed = None;
    if let Some(path) = &cfg.file {
        match open(path) {
            Ok(f) => layers.push(json(Mutex::new(f))),
            Err(e) => failed = Some((path, e)),
        }
    }
    tracing_subscriber::registry()
        .with(layers)
        .with(EnvFilter::new(filter))
        .init();
    if let Some((path, e)) = failed {
        tracing::warn!(error = %e, file = %path.display(), "log file not opened; logging to the console only");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(buf: &Buf) -> Vec<serde_json::Value> {
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The keys the log shipper config parses: flat, not nested under
    /// `fields`, one object per line, no colour codes.
    #[test]
    fn json_lines_are_flat_with_the_shipped_keys() {
        let buf = Buf::default();
        let b = buf.clone();
        let sub = tracing_subscriber::registry().with(vec![json(move || b.clone())]);
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "engine", order = "o-1", "order submitted");
            tracing::info_span!("run", tag = "tbmvqk2hzs").in_scope(|| {
                tracing::warn!(target: "exchange::binance", code = -2010, "order refused");
            });
        });
        let got = lines(&buf);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["level"], "INFO");
        assert_eq!(got[0]["target"], "engine");
        assert_eq!(got[0]["message"], "order submitted");
        assert_eq!(got[0]["order"], "o-1");
        assert!(got[0].get("fields").is_none(), "{}", got[0]);
        assert!(got[0].get("span").is_none(), "{}", got[0]);
        let ts = got[0]["timestamp"].as_str().unwrap();
        assert!(ts.contains('T') && ts.ends_with('Z'), "{ts}");
        assert_eq!(got[1]["level"], "WARN");
        assert_eq!(got[1]["code"], -2010);
        assert_eq!(got[1]["span"]["tag"], "tbmvqk2hzs");
        assert!(got[1].get("spans").is_none(), "{}", got[1]);
    }

    #[test]
    fn open_creates_the_directory_and_appends() {
        let dir = std::env::temp_dir().join(format!("tradebot-log-{}", std::process::id()));
        let path = dir.join("nested").join("run.jsonl");
        open(&path).unwrap().write_all(b"one\n").unwrap();
        open(&path).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
