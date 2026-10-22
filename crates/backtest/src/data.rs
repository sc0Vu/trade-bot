//! On-disk kline and instrument cache under `data/` (gitignored).
//!
//! Layout: `klines/<venue>/<SYMBOL>-<interval>.jsonl` (one candle per line,
//! money as strings) and `instruments/<venue>/<SYMBOL>.json`. Reads go back
//! through the core constructors, so a hand-edited or truncated cache is an
//! error, not a silently wrong backtest. After the first fetch, backtests
//! run fully offline.

use bot_core::{Candle, Instrument, InstrumentId, Interval, Timestamp};
use exchange::ExchangeError;
use exchange::binance::PublicClient;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Nothing cached; run `fetch-data` first.
    #[error("no cached data at {0}; run fetch-data first")]
    Missing(PathBuf),
    #[error("{path}:{line}: {reason}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> DataError + '_ {
    move |source| DataError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Root of the cache (normally `data/`).
#[derive(Clone, Debug)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn klines_path(&self, id: &InstrumentId, interval: Interval) -> PathBuf {
        self.root
            .join("klines")
            .join(id.venue().as_str())
            .join(format!("{}-{}.jsonl", id.symbol(), interval.code()))
    }

    fn instrument_path(&self, id: &InstrumentId) -> PathBuf {
        self.root
            .join("instruments")
            .join(id.venue().as_str())
            .join(format!("{}.json", id.symbol()))
    }

    /// Every cached candle for `id` at `interval`, oldest first. Empty if
    /// nothing is cached. Errors if a line doesn't decode to a valid candle
    /// of this instrument and interval, or open times don't strictly
    /// increase.
    pub fn read_klines(
        &self,
        id: &InstrumentId,
        interval: Interval,
    ) -> Result<Vec<Candle>, DataError> {
        let path = self.klines_path(id, interval);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&path)(e)),
        };
        let mut out: Vec<Candle> = Vec::new();
        for (n, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(io(&path))?;
            let corrupt = |reason: String| DataError::Corrupt {
                path: path.clone(),
                line: n + 1,
                reason,
            };
            let c: Candle = serde_json::from_str(&line).map_err(|e| corrupt(e.to_string()))?;
            if c.instrument != *id || c.interval != interval {
                return Err(corrupt("candle for another instrument or interval".into()));
            }
            if out.last().is_some_and(|prev| prev.open_time >= c.open_time) {
                return Err(corrupt("open times not strictly increasing".into()));
            }
            out.push(c);
        }
        Ok(out)
    }

    /// Replaces the cache for one series. Writes a temp file and renames it,
    /// so a crash mid-write never leaves a truncated cache.
    pub fn write_klines(
        &self,
        id: &InstrumentId,
        interval: Interval,
        candles: &[Candle],
    ) -> Result<(), DataError> {
        let path = self.klines_path(id, interval);
        let mut buf = Vec::new();
        for c in candles {
            serde_json::to_writer(&mut buf, c).map_err(|e| DataError::Corrupt {
                path: path.clone(),
                line: 0,
                reason: e.to_string(),
            })?;
            buf.push(b'\n');
        }
        write_atomic(&path, &buf)
    }

    /// Cached candles with `from <= open_time < to`. `Missing` if the series
    /// was never fetched.
    pub fn load_klines(
        &self,
        id: &InstrumentId,
        interval: Interval,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Candle>, DataError> {
        let all = self.read_klines(id, interval)?;
        if all.is_empty() {
            return Err(DataError::Missing(self.klines_path(id, interval)));
        }
        Ok(all
            .into_iter()
            .filter(|c| c.open_time >= from && c.open_time < to)
            .collect())
    }

    /// Cached reference data for `id`.
    pub fn read_instrument(&self, id: &InstrumentId) -> Result<Instrument, DataError> {
        let path = self.instrument_path(id);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DataError::Missing(path));
            }
            Err(e) => return Err(io(&path)(e)),
        };
        let i: Instrument = serde_json::from_str(&text).map_err(|e| DataError::Corrupt {
            path: path.clone(),
            line: 1,
            reason: e.to_string(),
        })?;
        if i.id != *id {
            return Err(DataError::Corrupt {
                path,
                line: 1,
                reason: "instrument file for another id".into(),
            });
        }
        Ok(i)
    }

    pub fn write_instrument(&self, i: &Instrument) -> Result<(), DataError> {
        let path = self.instrument_path(&i.id);
        let text = serde_json::to_vec_pretty(i).map_err(|e| DataError::Corrupt {
            path: path.clone(),
            line: 0,
            reason: e.to_string(),
        })?;
        write_atomic(&path, &text)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io(dir))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp).map_err(io(&tmp))?;
    f.write_all(bytes).map_err(io(&tmp))?;
    f.sync_all().map_err(io(&tmp))?;
    fs::rename(&tmp, path).map_err(io(path))
}

/// What one `fetch` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchSummary {
    pub downloaded: usize,
    pub cached: usize,
}

/// Brings the cache for `id` up to `[from, to)`: downloads the instrument's
/// filters if missing, and only the candles before the first or after the
/// last cached one. Gaps inside the cached range are left alone (they are
/// usually real exchange outages).
pub async fn fetch(
    client: &PublicClient,
    dir: &DataDir,
    id: &InstrumentId,
    interval: Interval,
    from: Timestamp,
    to: Timestamp,
) -> Result<FetchSummary, DataError> {
    if matches!(dir.read_instrument(id), Err(DataError::Missing(_))) {
        dir.write_instrument(&client.instrument(id.symbol()).await?)?;
    }
    let mut series: BTreeMap<Timestamp, Candle> = dir
        .read_klines(id, interval)?
        .into_iter()
        .map(|c| (c.open_time, c))
        .collect();
    let mut ranges = Vec::new();
    match (
        series.keys().next().copied(),
        series.keys().next_back().copied(),
    ) {
        (Some(first), Some(last)) => {
            if from < first {
                ranges.push((from, first));
            }
            let after = Timestamp::from_millis(last.as_millis() + 1);
            if after < to {
                ranges.push((after, to));
            }
        }
        _ => ranges.push((from, to)),
    }
    let mut downloaded = 0;
    for (start, end) in ranges {
        for c in client.klines(id.symbol(), interval, start, end).await? {
            downloaded += 1;
            series.insert(c.open_time, c);
        }
    }
    if downloaded > 0 {
        let all: Vec<Candle> = series.into_values().collect();
        dir.write_klines(id, interval, &all)?;
        return Ok(FetchSummary {
            downloaded,
            cached: all.len(),
        });
    }
    Ok(FetchSummary {
        downloaded,
        cached: series.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle, temp_dir};

    #[test]
    fn klines_round_trip_and_filter_by_range() {
        let dir = DataDir::new(temp_dir("roundtrip"));
        let id = btcusdt().id.clone();
        let cs: Vec<Candle> = (0..4)
            .map(|i| candle(&id, i, ["100", "101", "99", "100"]))
            .collect();
        dir.write_klines(&id, Interval::H1, &cs).unwrap();
        assert_eq!(dir.read_klines(&id, Interval::H1).unwrap(), cs);
        let mid = dir
            .load_klines(&id, Interval::H1, cs[1].open_time, cs[3].open_time)
            .unwrap();
        assert_eq!(mid, cs[1..3].to_vec());
    }

    #[test]
    fn missing_series_says_fetch_first() {
        let dir = DataDir::new(temp_dir("missing"));
        let err = dir
            .load_klines(
                &btcusdt().id,
                Interval::H1,
                Timestamp::from_millis(0),
                Timestamp::from_millis(1),
            )
            .unwrap_err();
        assert!(matches!(err, DataError::Missing(_)));
    }

    #[test]
    fn corrupt_or_unordered_cache_is_an_error() {
        let root = temp_dir("corrupt");
        let dir = DataDir::new(&root);
        let id = btcusdt().id.clone();
        let cs = vec![
            candle(&id, 1, ["1", "1", "1", "1"]),
            candle(&id, 0, ["1", "1", "1", "1"]),
        ];
        dir.write_klines(&id, Interval::H1, &cs).unwrap();
        assert!(matches!(
            dir.read_klines(&id, Interval::H1),
            Err(DataError::Corrupt { line: 2, .. })
        ));

        // A float where a price string belongs.
        let path = root.join("klines/binance/BTCUSDT-1h.jsonl");
        let line = serde_json::to_string(&cs[1])
            .unwrap()
            .replace("\"open\":\"1\"", "\"open\":1.0");
        fs::write(&path, line).unwrap();
        assert!(matches!(
            dir.read_klines(&id, Interval::H1),
            Err(DataError::Corrupt { line: 1, .. })
        ));
    }

    #[test]
    fn instrument_round_trip() {
        let dir = DataDir::new(temp_dir("instrument"));
        dir.write_instrument(&btcusdt()).unwrap();
        assert_eq!(dir.read_instrument(&btcusdt().id).unwrap(), btcusdt());
    }
}
