//! Perpetual-only cache and replay: funding history and margin data.
//!
//! Layout next to the klines: `funding/<venue>/<SYMBOL>.jsonl` (one
//! [`FundingRate`] per line, strictly increasing time, money as strings)
//! and `perp/<venue>/<SYMBOL>.json` ([`PerpData`]: the symbol's clearance
//! fee and, when fetched with a key, the account's margin brackets).
//!
//! Replay ([`funding_events`]): each settlement becomes a
//! `MarketEvent::Funding` at its own time. Where the venue's record has no
//! mark price (older history), the open of the bar containing the
//! settlement stands in for it: last price, not mark, an approximation
//! of a small amount (a rate of ~0.01% on a mark within a fraction of a
//! percent of last). A settlement with no bar at or before it is skipped:
//! no position can exist before the first bar.

use crate::DataError;
use crate::FetchSummary;
use crate::data::{DataDir, io, write_atomic};
use bot_core::margin::Brackets;
use bot_core::{Candle, Decimal, InstrumentId, MarketEvent, Timestamp};
use exchange::binance::PublicClient;
pub use exchange::binance::usdm::FundingRate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// What a perpetual backtest needs besides klines and filters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerpData {
    /// Liquidation clearance fee rate (`liquidationFee`).
    #[serde(with = "bot_core::decimal_str")]
    pub liquidation_fee: Decimal,
    /// The account's margin brackets; `None` until fetched with a key.
    pub brackets: Option<Brackets>,
}

impl DataDir {
    fn funding_path(&self, id: &InstrumentId) -> PathBuf {
        self.root
            .join("funding")
            .join(id.venue().as_str())
            .join(format!("{}.jsonl", id.symbol()))
    }

    fn perp_path(&self, id: &InstrumentId) -> PathBuf {
        self.root
            .join("perp")
            .join(id.venue().as_str())
            .join(format!("{}.json", id.symbol()))
    }

    /// Every cached funding settlement of `id`, oldest first; empty if
    /// none. Errors on a corrupt line or times not strictly increasing.
    pub fn read_funding(&self, id: &InstrumentId) -> Result<Vec<FundingRate>, DataError> {
        let path = self.funding_path(id);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&path)(e)),
        };
        let mut out: Vec<FundingRate> = Vec::new();
        for (n, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(io(&path))?;
            let corrupt = |reason: String| DataError::Corrupt {
                path: path.clone(),
                line: n + 1,
                reason,
            };
            let r: FundingRate = serde_json::from_str(&line).map_err(|e| corrupt(e.to_string()))?;
            if out.last().is_some_and(|prev| prev.ts >= r.ts) {
                return Err(corrupt("funding times not strictly increasing".into()));
            }
            out.push(r);
        }
        Ok(out)
    }

    /// Replaces `id`'s funding cache (atomically).
    pub fn write_funding(&self, id: &InstrumentId, rates: &[FundingRate]) -> Result<(), DataError> {
        let path = self.funding_path(id);
        let mut buf = Vec::new();
        for r in rates {
            serde_json::to_writer(&mut buf, r).map_err(|e| DataError::Corrupt {
                path: path.clone(),
                line: 0,
                reason: e.to_string(),
            })?;
            buf.push(b'\n');
        }
        write_atomic(&path, &buf)
    }

    /// `id`'s cached [`PerpData`]; `Missing` if never fetched.
    pub fn read_perp(&self, id: &InstrumentId) -> Result<PerpData, DataError> {
        let path = self.perp_path(id);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DataError::Missing(path));
            }
            Err(e) => return Err(io(&path)(e)),
        };
        serde_json::from_str(&text).map_err(|e| DataError::Corrupt {
            path,
            line: 1,
            reason: e.to_string(),
        })
    }

    pub fn write_perp(&self, id: &InstrumentId, data: &PerpData) -> Result<(), DataError> {
        let path = self.perp_path(id);
        let text = serde_json::to_vec_pretty(data).map_err(|e| DataError::Corrupt {
            path: path.clone(),
            line: 0,
            reason: e.to_string(),
        })?;
        write_atomic(&path, &text)
    }
}

/// Brings `id`'s funding cache up to `[from, to)`, downloading only
/// before the first or after the last cached settlement (as klines).
pub async fn fetch_funding(
    client: &PublicClient,
    dir: &DataDir,
    id: &InstrumentId,
    from: Timestamp,
    to: Timestamp,
) -> Result<FetchSummary, DataError> {
    let mut series: BTreeMap<Timestamp, FundingRate> = dir
        .read_funding(id)?
        .into_iter()
        .map(|r| (r.ts, r))
        .collect();
    let ranges = match (
        series.keys().next().copied(),
        series.keys().next_back().copied(),
    ) {
        (Some(first), Some(last)) => {
            let mut r = Vec::new();
            if from < first {
                r.push((from, first));
            }
            let after = Timestamp::from_millis(last.as_millis() + 1);
            if after < to {
                r.push((after, to));
            }
            r
        }
        _ => vec![(from, to)],
    };
    let mut downloaded = 0;
    for (start, end) in ranges {
        for r in client.funding_rates(id.symbol(), start, end).await? {
            downloaded += 1;
            series.insert(r.ts, r);
        }
    }
    let all: Vec<FundingRate> = series.into_values().collect();
    if downloaded > 0 {
        dir.write_funding(id, &all)?;
    }
    Ok(FetchSummary {
        downloaded,
        cached: all.len(),
    })
}

/// `rates` within `[from, to)` as `MarketEvent::Funding` of `id`, a
/// missing mark taken from `candles` (module docs). Also returns how many
/// marks were stood in for.
pub fn funding_events(
    id: &InstrumentId,
    rates: &[FundingRate],
    candles: &[Candle],
    from: Timestamp,
    to: Timestamp,
) -> (Vec<MarketEvent>, usize) {
    let mut bars: Vec<&Candle> = candles.iter().filter(|c| c.instrument == *id).collect();
    bars.sort_by_key(|c| c.open_time);
    let mut estimated = 0;
    let events = rates
        .iter()
        .filter(|r| r.ts >= from && r.ts < to)
        .filter_map(|r| {
            let mark = match r.mark {
                Some(m) => m,
                None => {
                    // The last bar opening at or before the settlement.
                    let i = bars.partition_point(|c| c.open_time <= r.ts);
                    let bar = bars.get(i.checked_sub(1)?)?;
                    estimated += 1;
                    if r.ts <= bar.close_time {
                        bar.open
                    } else {
                        bar.close
                    }
                }
            };
            Some(MarketEvent::Funding {
                instrument: id.clone(),
                rate: r.rate,
                mark,
                ts: r.ts,
            })
        })
        .collect();
    (events, estimated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{HOUR, btcusdt, candle, temp_dir};
    use bot_core::Price;
    use rust_decimal_macros::dec;

    fn rate(ts: i64, r: Decimal, mark: Option<&str>) -> FundingRate {
        FundingRate {
            ts: Timestamp::from_millis(ts),
            rate: r,
            mark: mark.map(|m| m.parse().unwrap()),
        }
    }

    #[test]
    fn funding_and_perp_data_round_trip_and_reject_disorder() {
        let dir = DataDir::new(temp_dir("funding"));
        let id = btcusdt().id.clone();
        let rs = vec![
            rate(15, dec!(0.0001), None),
            rate(8 * HOUR + 3, dec!(-0.0002), Some("100.5")),
        ];
        dir.write_funding(&id, &rs).unwrap();
        assert_eq!(dir.read_funding(&id).unwrap(), rs);
        dir.write_funding(&id, &[rs[1], rs[0]]).unwrap();
        assert!(matches!(
            dir.read_funding(&id),
            Err(DataError::Corrupt { line: 2, .. })
        ));
        assert!(matches!(dir.read_perp(&id), Err(DataError::Missing(_))));
        let data = PerpData {
            liquidation_fee: dec!(0.0125),
            brackets: None,
        };
        dir.write_perp(&id, &data).unwrap();
        assert_eq!(dir.read_perp(&id).unwrap(), data);
    }

    /// Bars t0..t1 (1h). A settlement 15 ms into t1 with no mark takes t1's
    /// open (105); one with a mark keeps it; one before t0 is skipped (no
    /// bar, no position); one past `to` is left out; one after the last
    /// bar's close takes that close (108).
    #[test]
    fn missing_marks_come_from_the_bar_around_the_settlement() {
        let id = btcusdt().id.clone();
        let bars = vec![
            candle(&id, 1, ["100", "101", "99", "100"]),
            candle(&id, 2, ["105", "109", "104", "108"]),
        ];
        let rs = vec![
            rate(HOUR / 2, dec!(0.1), None),
            rate(2 * HOUR + 15, dec!(0.0001), None),
            rate(2 * HOUR + 30, dec!(0.0002), Some("106")),
            rate(4 * HOUR, dec!(0.0003), None),
            rate(9 * HOUR, dec!(0.0004), None),
        ];
        let (ev, est) = funding_events(
            &id,
            &rs,
            &bars,
            Timestamp::from_millis(0),
            Timestamp::from_millis(5 * HOUR),
        );
        let marks: Vec<Price> = ev
            .iter()
            .map(|e| match e {
                MarketEvent::Funding { mark, .. } => *mark,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            marks,
            vec![
                Price::new(dec!(105)),
                Price::new(dec!(106)),
                Price::new(dec!(108))
            ]
        );
        assert_eq!(est, 2);
    }
}
