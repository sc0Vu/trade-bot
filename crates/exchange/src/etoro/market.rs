//! eToro instruments and candles.
//!
//! - [`EtoroClient::instruments`] (and `instrument` for one): `GET
//!   /api/v2/market-data/instruments?symbols=` (exact symbols, a list, one
//!   request per 50; case-insensitive), then the account's
//!   eligibility (`POST /api/v2/trading/info[/demo]/eligibility`) for the
//!   minimum exposure, unit type and whether the real asset can be bought
//!   (`leverageConfigs`: settlement `real`, direction `long`, leverage 1).
//! - [`EtoroClient::history`]: `GET /api/v1/data/instruments/{id}/candles`
//!   by date range (`from` inclusive, `to` exclusive, ≤ 2000 per page,
//!   newest first, cursor), bid side, prices as exact strings. Loaded once
//!   a day (newest bar ~a day old, checked 2026-11-18): history, not live.
//! - [`EtoroClient::recent`]: `GET /api/v1/market-data/instruments/{id}/
//!   history/candles/desc/{interval}/{n}` (≤ 1000), current to the minute,
//!   still-forming bar included (dropped here), prices JSON numbers read
//!   from the text. The live feed's source.
//!
//! Volume is often null (crypto): candles carry volume 0, quote volume 0.

use super::VENUE;
use super::client::{EtoroClient, Pool, ok};
use crate::ExchangeError;
use crate::max::public::raw_decimal;
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Decimal, Instrument, InstrumentId, InstrumentSpec,
    Interval, Notional, Price, Qty, Symbol, Timestamp, VenueId,
};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use std::str::FromStr;

/// Rows per page of the date-range candles (eToro's maximum).
const PAGE: usize = 2000;
/// Price step assumed where eToro publishes none.
const FINE_STEP: &str = "0.00000001";
/// Unit step of a fractional instrument: eToro publishes none; every
/// position it opened on the demo account (BTC, ETH, AAPL; 2026-11-18)
/// carried 6 decimals.
const UNIT_STEP: &str = "0.000001";

/// An eToro instrument as the bot trades it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EtoroInstrument {
    pub instrument: Instrument,
    /// eToro's numeric id (every trading and data path uses it).
    pub etoro_id: i64,
    /// eToro's asset type (`Stocks`, `Crypto`, `ETF`, ...).
    pub kind: String,
    /// eToro's `requiresW8Ben`: a real account must have the US tax form
    /// (W-8BEN) on file to trade it (US stocks).
    pub requires_w8ben: bool,
}

fn decode(what: impl Into<String>) -> ExchangeError {
    ExchangeError::Decode(what.into())
}

fn interval_code(i: Interval) -> Result<&'static str, ExchangeError> {
    match i {
        Interval::S1 => Err(ExchangeError::Rejected("eToro serves no 1s candles")),
        other => Ok(other.code()),
    }
}

fn recent_code(i: Interval) -> Result<&'static str, ExchangeError> {
    Ok(match i {
        Interval::M1 => "OneMinute",
        Interval::M5 => "FiveMinutes",
        Interval::M15 => "FifteenMinutes",
        Interval::H1 => "OneHour",
        Interval::H4 => "FourHours",
        Interval::D1 => "OneDay",
        Interval::S1 => return Err(ExchangeError::Rejected("eToro serves no 1s candles")),
    })
}

impl EtoroClient {
    /// eToro's id and type for exact `symbol`.
    pub async fn lookup(&self, symbol: &Symbol) -> Result<(i64, String, String), ExchangeError> {
        let body = self
            .get(
                Pool::MarketData,
                "/api/v2/market-data/instruments",
                &[("symbols", symbol.to_string())],
            )
            .await
            .map_err(|_| unknown(symbol))?;
        let v: Value =
            serde_json::from_str(&body).map_err(|e| decode(format!("instruments: {e}")))?;
        let hit = v["results"]
            .as_array()
            .and_then(|rs| {
                rs.iter().find(|r| {
                    r["symbol"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(symbol.as_str()))
                })
            })
            .ok_or_else(|| unknown(symbol))?;
        let id = hit["instrumentId"]
            .as_i64()
            .ok_or_else(|| decode("instrumentId"))?;
        let kind = hit["type"].as_str().unwrap_or_default().to_owned();
        let display = hit["displayName"].as_str().unwrap_or_default().to_owned();
        Ok((id, kind, display))
    }

    /// Whether eToro instrument `id` can be traded right now
    /// (`isCurrentlyTradable` from the instrument search: false while its
    /// market is closed — a London stock in the US afternoon — checked on
    /// demo 2026-11-18; `isExchangeOpen` means something else and is false
    /// for BTC).
    pub async fn tradable_now(&self, id: i64) -> Result<bool, ExchangeError> {
        let body = self
            .get(
                Pool::MarketData,
                "/api/v1/market-data/search",
                &[
                    ("fields", "instrumentId,isCurrentlyTradable".to_owned()),
                    ("instrumentId", id.to_string()),
                ],
            )
            .await?;
        parse_tradable(&body, id)
    }

    /// `symbol` as a tradable instrument for this account (module docs);
    /// [`instruments`](Self::instruments) for one.
    pub async fn instrument(&self, symbol: &Symbol) -> Result<EtoroInstrument, ExchangeError> {
        self.instruments(std::slice::from_ref(symbol))
            .await?
            .pop()
            .ok_or_else(|| unknown(symbol))
    }

    /// `symbols` as tradable instruments for this account, in order, in
    /// three requests per [`BATCH`] symbols (the lookup, the eligibility and
    /// the rates all take lists; checked on demo 2026-11-19), instead of
    /// three per symbol: a startup with many symbols otherwise bursts past
    /// eToro's rate limits.
    pub async fn instruments(
        &self,
        symbols: &[Symbol],
    ) -> Result<Vec<EtoroInstrument>, ExchangeError> {
        let mut out = Vec::with_capacity(symbols.len());
        for chunk in symbols.chunks(BATCH) {
            let list: Vec<String> = chunk.iter().map(Symbol::to_string).collect();
            let body = self
                .get(
                    Pool::MarketData,
                    "/api/v2/market-data/instruments",
                    &[("symbols", list.join(","))],
                )
                .await?;
            let found = parse_lookup(&body, chunk)?;
            let ids: Vec<i64> = found.iter().map(|(_, id, _)| *id).collect();
            let path = self.account().path(
                "/api/v2/trading/info/eligibility",
                "/api/v2/trading/info/demo/eligibility",
            );
            let reply = self
                .post(
                    Pool::Info,
                    &path,
                    &serde_json::json!({"instrumentIds": ids, "currency": "USD"}),
                    &uuid::Uuid::new_v4().to_string(),
                )
                .await?;
            let eligibility = ok(&path, reply)?;
            // Orders, fees and the books are in USD: an instrument priced in
            // another currency (BARC.L in pence, 7203.T in yen) would mix them.
            let ids_text: Vec<String> = ids.iter().map(i64::to_string).collect();
            let rates = self
                .get(
                    Pool::MarketData,
                    "/api/v1/market-data/instruments/rates",
                    &[("instrumentIds", ids_text.join(","))],
                )
                .await?;
            out.extend(assemble(&found, &eligibility, &rates)?);
        }
        Ok(out)
    }

    /// Closed candles of eToro instrument `id` (ours: `instrument`) with
    /// `start <= open_time < end`, oldest first (module docs).
    pub async fn history(
        &self,
        id: i64,
        instrument: &InstrumentId,
        interval: Interval,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let code = interval_code(interval)?;
        let bar = interval.as_millis();
        let now = now_millis();
        let mut out = Vec::new();
        let mut from = start.as_millis();
        // One request per window of PAGE bars (eToro also caps a slice at
        // 10,080 periods): no cursor needed within it.
        while from < end.as_millis() {
            let to = (from + bar * PAGE as i64).min(end.as_millis());
            let body = self
                .get(
                    Pool::MarketData,
                    &format!("/api/v1/data/instruments/{id}/candles"),
                    &[
                        ("interval", code.to_owned()),
                        ("from", iso(from)),
                        ("to", iso(to)),
                        ("limit", PAGE.to_string()),
                        ("side", "bid".to_owned()),
                    ],
                )
                .await?;
            let mut page = parse_history(&body, instrument, interval)?;
            page.retain(|c| c.close_time.as_millis() < now && c.open_time.as_millis() < to);
            out.extend(page);
            from = to;
        }
        out.sort_by_key(|c| c.open_time);
        out.dedup_by_key(|c| c.open_time);
        Ok(out)
    }

    /// The newest `n` (≤ 1000) closed candles of eToro instrument `id`,
    /// oldest first: the still-forming bar is dropped.
    pub async fn recent(
        &self,
        id: i64,
        instrument: &InstrumentId,
        interval: Interval,
        n: usize,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let code = recent_code(interval)?;
        let ask = (n + 1).min(1000);
        let body = self
            .get(
                Pool::MarketData,
                &format!("/api/v1/market-data/instruments/{id}/history/candles/desc/{code}/{ask}"),
                &[],
            )
            .await?;
        let mut bars = parse_recent(&body, instrument, interval)?;
        let now = now_millis();
        bars.retain(|c| c.close_time.as_millis() < now);
        bars.sort_by_key(|c| c.open_time);
        let drop = bars.len().saturating_sub(n);
        bars.drain(..drop);
        Ok(bars)
    }
}

fn unknown(symbol: &Symbol) -> ExchangeError {
    ExchangeError::UnknownInstrument(InstrumentId::new(
        VenueId::new(VENUE).unwrap_or_else(|_| unreachable!("valid venue id")),
        symbol.clone(),
    ))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Symbols per batched instrument lookup.
pub const BATCH: usize = 50;

/// eToro's id and type for each of `symbols` from one instruments reply
/// (exact symbol, ignoring case), in order; one missing = unknown.
pub(crate) fn parse_lookup(
    body: &str,
    symbols: &[Symbol],
) -> Result<Vec<(Symbol, i64, String)>, ExchangeError> {
    let v: Value = serde_json::from_str(body).map_err(|e| decode(format!("instruments: {e}")))?;
    let results = v["results"]
        .as_array()
        .ok_or_else(|| decode("instruments: no results"))?;
    symbols
        .iter()
        .map(|symbol| {
            let hit = results
                .iter()
                .find(|r| {
                    r["symbol"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(symbol.as_str()))
                })
                .ok_or_else(|| unknown(symbol))?;
            let id = hit["instrumentId"]
                .as_i64()
                .ok_or_else(|| decode("instrumentId"))?;
            let kind = hit["type"].as_str().unwrap_or_default().to_owned();
            Ok((symbol.clone(), id, kind))
        })
        .collect()
}

/// The instruments of one batch from its eligibility and rates replies.
pub(crate) fn assemble(
    found: &[(Symbol, i64, String)],
    eligibility: &str,
    rates: &str,
) -> Result<Vec<EtoroInstrument>, ExchangeError> {
    found
        .iter()
        .map(|(symbol, id, kind)| {
            let inst = parse_instrument(eligibility, symbol, *id, kind.clone())?;
            if !usd_priced(rates, *id)? {
                return Err(ExchangeError::Rejected(
                    "this eToro instrument is priced in a currency other than USD (not supported yet)",
                ));
            }
            Ok(inst)
        })
        .collect()
}

/// An eligibility reply → the instrument (module docs).
pub(crate) fn parse_instrument(
    body: &str,
    symbol: &Symbol,
    id: i64,
    kind: String,
) -> Result<EtoroInstrument, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply<'a> {
        #[serde(borrow)]
        eligibilities: Vec<Elig<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Elig<'a> {
        instrument_id: i64,
        #[serde(borrow)]
        min_position_exposure: &'a RawValue,
        allow_open_position: bool,
        #[serde(default)]
        requires_w8_ben: Option<bool>,
        units_quantity_type: String,
        leverage_configs: Vec<Config>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Config {
        settlement_type: String,
        direction: String,
        leverage_values: Vec<i64>,
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("eligibility: {e}")))?;
    let e = r
        .eligibilities
        .into_iter()
        .find(|e| e.instrument_id == id)
        .ok_or_else(|| unknown(symbol))?;
    // Casing differs between docs and replies: compare ignoring it.
    let real_long = e.leverage_configs.iter().any(|c| {
        c.settlement_type.eq_ignore_ascii_case("real")
            && c.direction.eq_ignore_ascii_case("long")
            && c.leverage_values.contains(&1)
    });
    if !real_long || !e.allow_open_position {
        return Err(ExchangeError::Rejected(
            "this eToro instrument can't be bought as the real asset (only as a CFD)",
        ));
    }
    let fine = Decimal::from_str(FINE_STEP).map_err(|_| decode("step"))?;
    let lot = if e
        .units_quantity_type
        .to_ascii_lowercase()
        .starts_with("fraction")
    {
        Decimal::from_str(UNIT_STEP).map_err(|_| decode("step"))?
    } else {
        Decimal::ONE
    };
    let instrument = Instrument::new(InstrumentSpec {
        id: InstrumentId::new(
            VenueId::new(VENUE).map_err(|e| decode(e.to_string()))?,
            symbol.clone(),
        ),
        class: AssetClass::Spot,
        base: Asset::new(symbol.as_str()).map_err(|e| decode(e.to_string()))?,
        quote: Asset::new("USD").map_err(|e| decode(e.to_string()))?,
        tick_size: Price::new(fine),
        lot_size: Qty::new(lot),
        min_qty: Qty::new(lot),
        min_notional: Notional::new(raw_decimal(e.min_position_exposure, "minPositionExposure")?),
        price_band: None,
        max_open_orders: None,
        max_open_stops: None,
    })
    .map_err(|e| decode(e.to_string()))?;
    Ok(EtoroInstrument {
        instrument,
        etoro_id: id,
        kind,
        requires_w8ben: e.requires_w8_ben.unwrap_or(false),
    })
}

/// Whether a rates reply prices instrument `id` in USD: both conversion
/// rates exactly 1 (BARC.L: 0.013219, pence → USD; checked 2026-11-18).
pub(crate) fn usd_priced(body: &str, id: i64) -> Result<bool, ExchangeError> {
    #[derive(Deserialize)]
    struct Reply<'a> {
        #[serde(borrow)]
        rates: Vec<Rate<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Rate<'a> {
        #[serde(rename = "instrumentID")]
        instrument_id: i64,
        #[serde(borrow)]
        conversion_rate_ask: &'a RawValue,
        #[serde(borrow)]
        conversion_rate_bid: &'a RawValue,
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("rates: {e}")))?;
    let rate = r
        .rates
        .iter()
        .find(|x| x.instrument_id == id)
        .ok_or_else(|| decode(format!("rates: none for instrument {id}")))?;
    Ok(
        raw_decimal(rate.conversion_rate_ask, "conversionRateAsk")? == Decimal::ONE
            && raw_decimal(rate.conversion_rate_bid, "conversionRateBid")? == Decimal::ONE,
    )
}

/// A search reply → instrument `id`'s `isCurrentlyTradable`.
pub(crate) fn parse_tradable(body: &str, id: i64) -> Result<bool, ExchangeError> {
    let v: Value = serde_json::from_str(body).map_err(|e| decode(format!("search: {e}")))?;
    v["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|i| i["instrumentId"].as_i64() == Some(id))
        })
        .and_then(|i| i["isCurrentlyTradable"].as_bool())
        .ok_or_else(|| decode(format!("search: no tradability for instrument {id}")))
}

fn candle(
    instrument: &InstrumentId,
    interval: Interval,
    open_ms: i64,
    [o, h, l, c]: [Decimal; 4],
    volume: Decimal,
) -> Result<Candle, ExchangeError> {
    Candle::new(CandleSpec {
        instrument: instrument.clone(),
        interval,
        open_time: Timestamp::from_millis(open_ms),
        close_time: Timestamp::from_millis(open_ms + interval.as_millis() - 1),
        open: Price::new(o),
        high: Price::new(h),
        low: Price::new(l),
        close: Price::new(c),
        volume: Qty::new(volume),
        quote_volume: Notional::ZERO,
    })
    .map_err(|e| decode(e.to_string()))
}

/// A date-range page (prices strings, newest first).
pub(crate) fn parse_history(
    body: &str,
    instrument: &InstrumentId,
    interval: Interval,
) -> Result<Vec<Candle>, ExchangeError> {
    let v: Value = serde_json::from_str(body).map_err(|e| decode(format!("candles: {e}")))?;
    let rows = v["results"]
        .as_array()
        .ok_or_else(|| decode("candles: no results"))?;
    rows.iter()
        .map(|r| {
            let d = |k: &str| {
                r[k].as_str()
                    .and_then(|s| Decimal::from_str_exact(s).ok())
                    .ok_or_else(|| decode(format!("candle {k}")))
            };
            let t = r["time"].as_str().ok_or_else(|| decode("candle time"))?;
            let volume = r["volume"]
                .as_str()
                .and_then(|s| Decimal::from_str_exact(s).ok())
                .unwrap_or(Decimal::ZERO);
            candle(
                instrument,
                interval,
                parse_iso(t)?,
                [d("open")?, d("high")?, d("low")?, d("close")?],
                volume,
            )
        })
        .collect()
}

/// A recent-candles reply (prices JSON numbers, read from the text).
pub(crate) fn parse_recent(
    body: &str,
    instrument: &InstrumentId,
    interval: Interval,
) -> Result<Vec<Candle>, ExchangeError> {
    #[derive(Deserialize)]
    struct Reply<'a> {
        #[serde(borrow)]
        candles: Vec<Group<'a>>,
    }
    #[derive(Deserialize)]
    struct Group<'a> {
        #[serde(borrow)]
        candles: Vec<Row<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Row<'a> {
        from_date: String,
        #[serde(borrow)]
        open: &'a RawValue,
        #[serde(borrow)]
        high: &'a RawValue,
        #[serde(borrow)]
        low: &'a RawValue,
        #[serde(borrow)]
        close: &'a RawValue,
        #[serde(borrow, default)]
        volume: Option<&'a RawValue>,
    }
    let r: Reply = serde_json::from_str(body).map_err(|e| decode(format!("candles: {e}")))?;
    let mut out = Vec::new();
    for g in r.candles {
        for row in g.candles {
            let volume = match row.volume {
                Some(v) if v.get() != "null" => raw_decimal(v, "volume")?,
                _ => Decimal::ZERO,
            };
            out.push(candle(
                instrument,
                interval,
                parse_iso(&row.from_date)?,
                [
                    raw_decimal(row.open, "open")?,
                    raw_decimal(row.high, "high")?,
                    raw_decimal(row.low, "low")?,
                    raw_decimal(row.close, "close")?,
                ],
                volume,
            )?);
        }
    }
    Ok(out)
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a UTC millisecond time.
pub(crate) fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// UTC milliseconds of `YYYY-MM-DDTHH:MM:SS[.fff…][Z]` (eToro's times).
pub(crate) fn parse_iso(s: &str) -> Result<i64, ExchangeError> {
    let bad = || decode(format!("time {s:?}"));
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T').ok_or_else(bad)?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, dd) = match (d.next(), d.next(), d.next()) {
        (Some(Ok(y)), Some(Ok(m)), Some(Ok(dd))) => (y, m, dd),
        _ => return Err(bad()),
    };
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':').map(|p| p.parse::<i64>());
    let (h, mi, se) = match (t.next(), t.next(), t.next()) {
        (Some(Ok(h)), Some(Ok(mi)), Some(Ok(se))) => (h, mi, se),
        _ => return Err(bad()),
    };
    let ms = if frac.is_empty() {
        0
    } else {
        format!("{:0<3}", &frac[..frac.len().min(3)])
            .parse::<i64>()
            .map_err(|_| bad())?
    };
    Ok(((days_from_civil(y, m, dd) * 86_400 + h * 3600 + mi * 60 + se) * 1000) + ms)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day count since 1970-01-01 (Hinnant).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn aapl() -> InstrumentId {
        "etoro:AAPL".parse().unwrap()
    }

    #[test]
    fn iso_times_round_trip() {
        // 2026-11-17T04:00:00Z = 1,794,888,000,000 ms.
        assert_eq!(
            parse_iso("2026-11-17T04:00:00Z").unwrap(),
            1_794_888_000_000
        );
        assert_eq!(iso(1_794_888_000_000), "2026-11-17T04:00:00Z");
        assert_eq!(
            parse_iso("2026-11-17T18:03:30.297").unwrap(),
            1_794_938_610_297
        );
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            parse_iso("2024-02-29T00:00:00Z").unwrap(),
            1_709_164_800_000
        );
        assert!(parse_iso("yesterday").is_err());
    }

    /// AAPL's demo eligibility as returned 2026-11-18 (trimmed): real long
    /// at 1x, fractional units, $10 minimum → lot 1e-6 (observed), min notional 10.
    /// BTC.Fut-like: CFD only → refused.
    #[test]
    fn eligibility_makes_a_real_asset_instrument() {
        let body = r#"{"currency":"usd","eligibilities":[{"instrumentId":1001,"symbol":"AAPL","minPositionExposure":10.0,"maxUnitsPerOrder":5841.0,"allowOpenPosition":true,"unitsQuantityType":"fractional","leverageConfigs":[{"settlementType":"real","direction":"long","leverageValues":[1]},{"settlementType":"cfd","direction":"short","leverageValues":[1,2,5]}]}]}"#;
        let e = parse_instrument(body, &"AAPL".parse().unwrap(), 1001, "Stocks".into()).unwrap();
        let i = &e.instrument;
        assert_eq!(i.id, aapl());
        assert_eq!(i.class, AssetClass::Spot);
        assert_eq!(i.quote.as_str(), "USD");
        assert_eq!(
            (i.lot_size.get(), i.min_notional.get()),
            (dec!(0.000001), dec!(10))
        );
        assert_eq!(e.etoro_id, 1001);
        assert!(!e.requires_w8ben);
        let w8 = body.replace(
            r#""allowOpenPosition":true"#,
            r#""allowOpenPosition":true,"requiresW8Ben":true"#,
        );
        assert!(
            parse_instrument(&w8, &"AAPL".parse().unwrap(), 1001, "Stocks".into())
                .unwrap()
                .requires_w8ben
        );
        let cfd = r#"{"eligibilities":[{"instrumentId":315,"minPositionExposure":50,"allowOpenPosition":true,"unitsQuantityType":"Whole","leverageConfigs":[{"settlementType":"CFD","direction":"LONG","leverageValues":[2,5]}]}]}"#;
        assert!(parse_instrument(cfd, &"BTCFUT".parse().unwrap(), 315, "CFD".into()).is_err());
    }

    /// Real replies (2026-11-18): date-range bars carry strings, recent
    /// bars numbers; both exact; volume null → 0; close = open + 1h − 1ms.
    #[test]
    fn candles_are_read_exactly() {
        let hist = r#"{"instrumentId":100000,"interval":"1h","results":[{"time":"2026-11-17T04:00:00Z","open":"64995.87","high":"65034.96","low":"64852.25","close":"64981.57","volume":null}]}"#;
        let c = &parse_history(hist, &aapl(), Interval::H1).unwrap()[0];
        assert_eq!(c.open.get(), dec!(64995.87));
        assert_eq!(c.close_time.as_millis(), 1_794_888_000_000 + 3_600_000 - 1);
        assert_eq!(c.volume.get(), dec!(0));
        let recent = r#"{"interval":"OneMinute","candles":[{"instrumentId":1001,"candles":[{"instrumentID":1001,"fromDate":"2026-11-17T18:04:00Z","open":350.67,"high":350.82,"low":350.67,"close":350.82,"volume":15009.0},{"instrumentID":1001,"fromDate":"2026-11-17T18:03:00Z","open":350.74,"high":350.74,"low":350.61,"close":350.67,"volume":null}]}]}"#;
        let bars = parse_recent(recent, &aapl(), Interval::M1).unwrap();
        assert_eq!(bars[0].close.get(), dec!(350.82));
        assert_eq!(bars[0].volume.get(), dec!(15009.0));
        assert_eq!(bars[1].volume.get(), dec!(0));
    }

    /// Real replies (2026-11-18, 18:39 UTC): BARC.L closed, AAPL open.
    #[test]
    fn tradability_comes_from_is_currently_tradable() {
        let closed = r#"{"page":1,"items":[{"internalSymbolFull":"BARC.L","instrumentId":2013,"isCurrentlyTradable":false,"isExchangeOpen":false}]}"#;
        assert!(!parse_tradable(closed, 2013).unwrap());
        let open = r#"{"items":[{"instrumentId":1001,"isCurrentlyTradable":true,"isExchangeOpen":false}]}"#;
        assert!(parse_tradable(open, 1001).unwrap());
        assert!(parse_tradable(open, 7).is_err());
    }

    /// Real rates (2026-11-18): AAPL converts at 1.0 (USD), BARC.L at
    /// 0.013219 (pence).
    #[test]
    fn only_usd_priced_instruments_pass() {
        let body = r#"{"rates":[{"instrumentID":1001,"ask":350.93,"bid":350.86,"conversionRateAsk":1.0,"conversionRateBid":1.0},{"instrumentID":2013,"ask":417.9,"conversionRateAsk":0.013219,"conversionRateBid":0.013218}]}"#;
        assert!(usd_priced(body, 1001).unwrap());
        assert!(!usd_priced(body, 2013).unwrap());
        assert!(usd_priced(body, 5).is_err());
    }

    /// One lookup, one eligibility and one rates reply for three symbols:
    /// each instrument comes out in the order asked, matched by id; a
    /// symbol missing from the lookup is unknown; a non-USD one refused.
    #[test]
    fn a_batch_resolves_every_symbol_from_three_replies() {
        let sym = |s: &str| -> Symbol { s.parse().unwrap() };
        let lookup = r#"{"results":[
            {"symbol":"MSFT","instrumentId":1004,"type":"Stocks"},
            {"symbol":"AAPL","instrumentId":1001,"type":"Stocks"},
            {"symbol":"QQQ","instrumentId":3006,"type":"ETF"}]}"#;
        let found = parse_lookup(lookup, &[sym("AAPL"), sym("qqq"), sym("MSFT")]).unwrap();
        let ids: Vec<i64> = found.iter().map(|(_, id, _)| *id).collect();
        assert_eq!(ids, vec![1001, 3006, 1004]);
        assert!(parse_lookup(lookup, &[sym("AAPL"), sym("NOPE")]).is_err());

        let elig = |id: i64, sym: &str| {
            format!(
                r#"{{"instrumentId":{id},"symbol":"{sym}","minPositionExposure":10.0,"allowOpenPosition":true,"unitsQuantityType":"fractional","leverageConfigs":[{{"settlementType":"real","direction":"long","leverageValues":[1]}}]}}"#
            )
        };
        let eligibility = format!(
            r#"{{"currency":"usd","eligibilities":[{},{},{}]}}"#,
            elig(1004, "MSFT"),
            elig(3006, "QQQ"),
            elig(1001, "AAPL")
        );
        let rate = |id: i64, c: &str| {
            format!(
                r#"{{"instrumentID":{id},"ask":1.0,"conversionRateAsk":{c},"conversionRateBid":{c}}}"#
            )
        };
        let usd = format!(
            r#"{{"rates":[{},{},{}]}}"#,
            rate(1001, "1.0"),
            rate(3006, "1.0"),
            rate(1004, "1.0")
        );
        let got = assemble(&found, &eligibility, &usd).unwrap();
        let names: Vec<String> = got.iter().map(|e| e.instrument.id.to_string()).collect();
        assert_eq!(names, vec!["etoro:AAPL", "etoro:QQQ", "etoro:MSFT"]);
        let pence = format!(
            r#"{{"rates":[{},{},{}]}}"#,
            rate(1001, "1.0"),
            rate(3006, "0.013"),
            rate(1004, "1.0")
        );
        assert!(assemble(&found, &eligibility, &pence).is_err());
    }
}
