//! Delistings in a backtest, without lookahead.
//!
//! A live run learns of a delisting when the venue announces it
//! (`MarketEvent::Delisting`), then refuses new buys of the coin and
//! sells it before the delisting. A backtest must learn of it at the same
//! moment and no earlier: working out "this coin delists" from where its
//! data ends would act on the future (sell a coin a week before a halt
//! nobody had announced). So it has two sources:
//!
//! - [`read_delistings`]: a committed reference file of announced
//!   delistings, each with its announcement time, delisting time and a
//!   source. The event is replayed at the announcement, as live.
//! - [`cut_at_gaps`]: a series that stops for longer than a gap, or ends
//!   before the backtest does, is cut at its last bar, and a delisting
//!   takes effect right after it with no notice: a halt nobody announced.
//!   A holding then is stuck at its last price, as it was for real
//!   holders. This also keeps a symbol the venue reused for another coin
//!   (LUNAUSDT: LUNA, then LUNA 2.0 after an 18-day gap) from splicing two
//!   coins into one series.

use crate::DataError;
use bot_core::{Candle, InstrumentId, MarketEvent, Symbol, Timestamp, VenueId};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// One announced delisting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delisting {
    pub instrument: InstrumentId,
    /// When the venue announced it: the event's time.
    pub announced: Timestamp,
    /// When trading stops.
    pub at: Timestamp,
    /// Where the dates come from (an announcement URL).
    pub source: String,
}

impl Delisting {
    /// As the event a live feed would have produced.
    pub fn event(&self) -> MarketEvent {
        MarketEvent::Delisting {
            instrument: self.instrument.clone(),
            at: Some(self.at),
            ts: self.announced,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    delisting: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    symbol: Symbol,
    announced: toml::value::Datetime,
    at: toml::value::Datetime,
    source: String,
}

/// Reads `path`, a TOML list of `[[delisting]]` tables (`symbol`,
/// `announced`, `at` as UTC datetimes like `2024-02-06T09:00:00Z`,
/// `source`), for `venue`. An entry announced after it delists, with a
/// datetime not in UTC, or without a source is an error: the file is
/// evidence and must read as such.
pub fn read_delistings(path: &Path, venue: &VenueId) -> Result<Vec<Delisting>, DataError> {
    let corrupt = |reason: String| DataError::Corrupt {
        path: path.to_owned(),
        line: 0,
        reason,
    };
    let text = std::fs::read_to_string(path).map_err(|source| DataError::Io {
        path: path.to_owned(),
        source,
    })?;
    let file: File = toml::from_str(&text).map_err(|e| corrupt(e.message().to_owned()))?;
    file.delisting
        .into_iter()
        .map(|e| {
            let announced = utc_millis(&e.announced).ok_or_else(|| {
                corrupt(format!("{}: announced must be a UTC datetime", e.symbol))
            })?;
            let at = utc_millis(&e.at)
                .ok_or_else(|| corrupt(format!("{}: at must be a UTC datetime", e.symbol)))?;
            if announced > at {
                return Err(corrupt(format!("{}: announced after it delists", e.symbol)));
            }
            if e.source.trim().is_empty() {
                return Err(corrupt(format!("{}: no source", e.symbol)));
            }
            Ok(Delisting {
                instrument: InstrumentId::new(venue.clone(), e.symbol),
                announced,
                at,
                source: e.source,
            })
        })
        .collect()
}

/// Milliseconds since the epoch of a TOML datetime with date, time and a
/// `Z` (or +00:00) offset; `None` otherwise.
fn utc_millis(d: &toml::value::Datetime) -> Option<Timestamp> {
    use toml::value::Offset;
    let (date, time) = (d.date?, d.time?);
    match d.offset? {
        Offset::Z => {}
        Offset::Custom { minutes: 0 } => {}
        Offset::Custom { .. } => return None,
    }
    let days = days_from_civil(i64::from(date.year), date.month.into(), date.day.into());
    let secs = days * 86_400
        + i64::from(time.hour) * 3600
        + i64::from(time.minute) * 60
        + i64::from(time.second.unwrap_or(0));
    Some(Timestamp::from_millis(
        secs * 1000 + i64::from(time.nanosecond.unwrap_or(0) / 1_000_000),
    ))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Cuts each instrument's candles at its first gap longer than `max_gap`
/// (time from one bar's close to the next bar's open), and at its end if
/// that comes more than `max_gap` before `to`; returns the candles kept
/// and, per cut, a delisting taking effect 1 ms after the last bar kept,
/// announced at its close: no notice. See the module docs.
pub fn cut_at_gaps(
    candles: Vec<Candle>,
    max_gap: Duration,
    to: Timestamp,
) -> (Vec<Candle>, Vec<MarketEvent>) {
    let gap = i64::try_from(max_gap.as_millis()).unwrap_or(i64::MAX);
    let mut by: BTreeMap<InstrumentId, Vec<Candle>> = BTreeMap::new();
    for c in candles {
        by.entry(c.instrument.clone()).or_default().push(c);
    }
    let mut kept = Vec::new();
    let mut events = Vec::new();
    for (id, mut series) in by {
        series.sort_by_key(|c| c.open_time);
        let cut = series
            .windows(2)
            .position(|w| w[1].open_time.as_millis() - w[0].close_time.as_millis() > gap)
            .map(|i| i + 1)
            .or_else(|| {
                let last = series.last()?;
                (to.as_millis() - last.close_time.as_millis() > gap).then_some(series.len())
            });
        if let Some(n) = cut {
            series.truncate(n);
            if let Some(last) = series.last() {
                events.push(MarketEvent::Delisting {
                    instrument: id,
                    at: Some(Timestamp::from_millis(last.close_time.as_millis() + 1)),
                    ts: last.close_time,
                });
            }
        }
        kept.extend(series);
    }
    (kept, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};

    const HOUR: i64 = 3_600_000;

    #[test]
    fn utc_datetimes_read_exactly() {
        let d: toml::value::Datetime = "2024-02-20T03:00:00Z".parse().unwrap();
        assert_eq!(
            utc_millis(&d),
            Some(Timestamp::from_millis(1_708_398_000_000))
        );
        let epoch: toml::value::Datetime = "1970-01-01T00:00:00.250Z".parse().unwrap();
        assert_eq!(utc_millis(&epoch), Some(Timestamp::from_millis(250)));
        let local: toml::value::Datetime = "2024-02-20T03:00:00+08:00".parse().unwrap();
        assert_eq!(utc_millis(&local), None);
        let date_only: toml::value::Datetime = "2024-02-20".parse().unwrap();
        assert_eq!(utc_millis(&date_only), None);
    }

    #[test]
    fn reads_the_reference_file_and_refuses_bad_entries() {
        let dir = std::env::temp_dir().join(format!("tb-delist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.toml");
        let venue: VenueId = "binance".parse().unwrap();
        let entry = |announced: &str, source: &str| {
            format!(
                "[[delisting]]\nsymbol = \"SRMUSDT\"\nannounced = {announced}\n\
                 at = 2022-11-28T03:00:00Z\nsource = \"{source}\"\n"
            )
        };
        std::fs::write(&path, entry("2022-11-15T00:00:00Z", "https://x")).unwrap();
        let got = read_delistings(&path, &venue).unwrap();
        assert_eq!(got[0].instrument, "binance:SRMUSDT".parse().unwrap());
        assert_eq!(got[0].at, Timestamp::from_millis(1_669_604_400_000));
        assert_eq!(
            got[0].event(),
            MarketEvent::Delisting {
                instrument: "binance:SRMUSDT".parse().unwrap(),
                at: Some(Timestamp::from_millis(1_669_604_400_000)),
                ts: Timestamp::from_millis(1_668_470_400_000),
            }
        );
        for bad in [
            entry("2022-11-29T00:00:00Z", "https://x"),
            entry("2022-11-15T00:00:00Z", " "),
            entry("2022-11-15", "https://x"),
        ] {
            std::fs::write(&path, bad.clone()).unwrap();
            assert!(read_delistings(&path, &venue).is_err(), "{bad}");
        }
    }

    /// Bars 0..3, then a gap (next opens at 10h: 7h after bar 2 closes),
    /// then 10..12. With a 2h limit the series is cut after bar 2 and
    /// delists right after its close; with a 10h limit nothing is cut.
    /// A series ending well before `to` is cut at its end.
    #[test]
    fn a_long_gap_or_an_early_end_cuts_the_series() {
        let id = btcusdt().id.clone();
        let bars: Vec<Candle> = [0, 1, 2, 10, 11]
            .into_iter()
            .map(|i| candle(&id, i, ["1"; 4]))
            .collect();
        let to = Timestamp::from_millis(12 * HOUR);
        let (kept, events) = cut_at_gaps(bars.clone(), Duration::from_secs(2 * 3600), to);
        assert_eq!(kept.len(), 3);
        assert_eq!(
            events,
            vec![MarketEvent::Delisting {
                instrument: id.clone(),
                at: Some(Timestamp::from_millis(3 * HOUR)),
                ts: Timestamp::from_millis(3 * HOUR - 1),
            }]
        );
        let (kept, events) = cut_at_gaps(bars.clone(), Duration::from_secs(10 * 3600), to);
        assert_eq!((kept.len(), events.len()), (5, 0));
        let (kept, events) = cut_at_gaps(
            bars,
            Duration::from_secs(10 * 3600),
            Timestamp::from_millis(40 * HOUR),
        );
        assert_eq!((kept.len(), events.len()), (5, 1), "ends 28h early");
    }
}
