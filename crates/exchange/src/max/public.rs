//! Keyless MAX REST: markets (instrument filters) and klines.
//!
//! MAX sends market minimums and kline prices as bare JSON numbers
//! (`"min_base_amount":0.0001`, `[1793178000,81522.59,…]`). They are read
//! as the number's text and parsed exactly into `Decimal`, never through a
//! float (invariant 1); `serde_json`'s raw values keep the text as sent.

use crate::ExchangeError;
use crate::rate_limit::RateLimiter;
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Decimal, Instrument, InstrumentId, InstrumentSpec,
    Interval, Notional, Price, Qty, Symbol, Timestamp, VenueId,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// MAX's largest kline page (`limit` above it is refused, checked
/// 2026-10-28).
const KLINE_LIMIT: usize = 10_000;
/// After a 429 from MAX, hold every request this long.
const BACKOFF: Duration = Duration::from_secs(60);

/// Client for keyless MAX endpoints.
pub struct MaxPublicClient {
    http: reqwest::Client,
    base: String,
    venue: VenueId,
    limiter: Arc<RateLimiter>,
}

impl MaxPublicClient {
    /// `base` is the REST host, normally [`super::REST_URL`]. Instruments are
    /// tagged with venue [`super::VENUE`]. Requests time out after
    /// [`crate::binance::DEFAULT_HTTP_TIMEOUT`].
    pub fn new(base: impl Into<String>) -> Self {
        Self::with_timeout(base, crate::binance::DEFAULT_HTTP_TIMEOUT)
    }

    /// [`new`](Self::new) with a request timeout of `timeout`.
    pub fn with_timeout(base: impl Into<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // Fails only if TLS can't start; `Client::new` then panics too.
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            venue: VenueId::new(super::VENUE).unwrap_or_else(|_| unreachable!("valid venue id")),
            limiter: Arc::new(RateLimiter::new(super::default_limits())),
        }
    }

    /// Paces requests through `limiter` instead of a private one (share it
    /// with every client of the same host).
    pub fn with_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.limiter = limiter;
        self
    }

    /// The body of a GET, as text: callers parse it with raw numbers kept.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<String, ExchangeError> {
        self.limiter
            .acquire(1, 0)
            .await
            .map_err(|e| ExchangeError::RateLimited(e.to_string()))?;
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .send()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| ExchangeError::Http(e.without_url().to_string()))?;
        if status.as_u16() == 429 {
            // MAX bans an IP that keeps going past a 429.
            self.limiter.block_for(BACKOFF);
            metrics::counter!(crate::metrics::RATE_LIMITED).increment(1);
        }
        if !status.is_success() {
            return Err(ExchangeError::Http(format!(
                "{path}: HTTP {status}: {body}"
            )));
        }
        Ok(body)
    }

    /// Every MAX market, with its filters and status.
    pub async fn markets(&self) -> Result<Vec<Market>, ExchangeError> {
        let body = self.get("/api/v3/markets", &[]).await?;
        parse_markets(&body, &self.venue)
    }

    /// `symbol`'s filters (`BTCUSDT` = market `btcusdt`). An unknown market
    /// is [`ExchangeError::UnknownInstrument`].
    pub async fn instrument(&self, symbol: &Symbol) -> Result<Instrument, ExchangeError> {
        self.markets()
            .await?
            .into_iter()
            .find(|m| m.instrument.id.symbol() == symbol)
            .map(|m| m.instrument)
            .ok_or_else(|| {
                ExchangeError::UnknownInstrument(InstrumentId::new(
                    self.venue.clone(),
                    symbol.clone(),
                ))
            })
    }

    /// Closed candles for `symbol` with `start <= open_time < end`, oldest
    /// first, paginated by 10,000 (MAX's largest page). The still-forming candle is
    /// never returned (invariant 2). Quote volume is an estimate: see
    /// [`parse_klines`].
    pub async fn klines(
        &self,
        symbol: &Symbol,
        interval: Interval,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let id = InstrumentId::new(self.venue.clone(), symbol.clone());
        let period = period_minutes(interval)?;
        let now = now_millis();
        let mut out: Vec<Candle> = Vec::new();
        // `timestamp` is in seconds and returns bars opening at or after it.
        let mut cursor = start.as_millis().div_euclid(1000);
        loop {
            let body = self
                .get(
                    "/api/v3/k",
                    &[
                        ("market", symbol.as_str().to_lowercase()),
                        ("period", period.to_string()),
                        ("timestamp", cursor.to_string()),
                        ("limit", KLINE_LIMIT.to_string()),
                    ],
                )
                .await?;
            let page = parse_klines(&body, &id, interval)?;
            let full = page.len() == KLINE_LIMIT;
            let Some(last) = page.last().map(|c| c.open_time.as_millis()) else {
                break;
            };
            out.extend(page.into_iter().filter(|c| {
                c.open_time >= start && c.open_time < end && c.close_time.as_millis() < now
            }));
            if !full || last + interval.as_millis() >= end.as_millis() {
                break;
            }
            cursor = (last + interval.as_millis()).div_euclid(1000);
        }
        Ok(out)
    }
}

/// MAX's `period` for `interval`, in minutes. MAX has no 1s klines.
fn period_minutes(interval: Interval) -> Result<i64, ExchangeError> {
    match interval {
        Interval::S1 => Err(ExchangeError::Decode(
            "MAX serves no 1s klines; use 1m or longer".into(),
        )),
        i => Ok(i.as_millis() / 60_000),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn decode(what: impl Into<String>) -> ExchangeError {
    ExchangeError::Decode(what.into())
}

/// Exact decimal from a JSON number's text (`0.0001`, `28.0`, `1e-05`) or a
/// JSON string holding one. Never through `f64`.
pub(crate) fn raw_decimal(raw: &RawValue, what: &str) -> Result<Decimal, ExchangeError> {
    let text = raw.get().trim_matches('"');
    let parsed = if text.contains(['e', 'E']) {
        Decimal::from_scientific(text)
    } else {
        Decimal::from_str_exact(text)
    };
    parsed.map_err(|_| decode(format!("{what}: {text:?} is not a decimal")))
}

/// One step of a market's precision: `10^-precision`. MAX documents
/// precision as decimal places that "can be negative": 8 → 0.00000001,
/// 0 → 1, -3 → 1000.
pub fn step(precision: i64) -> Result<Decimal, ExchangeError> {
    match precision {
        0..=18 => Ok(Decimal::new(1, precision as u32)),
        -18..=-1 => Ok(Decimal::from(10_i64.pow((-precision) as u32))),
        p => Err(decode(format!("precision {p} out of range"))),
    }
}

/// One market of `/api/v3/markets`.
pub struct Market {
    pub instrument: Instrument,
    /// `active`, `cancel-only` (no new orders) or `suspended`.
    pub status: String,
}

#[derive(Deserialize)]
struct RawMarket<'a> {
    id: String,
    status: String,
    base_unit: String,
    base_unit_precision: i64,
    #[serde(borrow)]
    min_base_amount: &'a RawValue,
    quote_unit: String,
    quote_unit_precision: i64,
    #[serde(borrow)]
    min_quote_amount: &'a RawValue,
}

/// Parses `/api/v3/markets`. Tick = one step of `quote_unit_precision`,
/// lot = one step of `base_unit_precision`, minimums as published. MAX
/// publishes no price band or open-order caps.
pub fn parse_markets(body: &str, venue: &VenueId) -> Result<Vec<Market>, ExchangeError> {
    let raw: Vec<RawMarket> =
        serde_json::from_str(body).map_err(|e| decode(format!("markets: {e}")))?;
    raw.into_iter()
        .map(|m| {
            let asset = |unit: &str| {
                Asset::new(unit.to_uppercase()).map_err(|e| decode(format!("market {}: {e}", m.id)))
            };
            let symbol = Symbol::new(m.id.to_uppercase())
                .map_err(|e| decode(format!("market {}: {e}", m.id)))?;
            let instrument = Instrument::new(InstrumentSpec {
                id: InstrumentId::new(venue.clone(), symbol),
                class: AssetClass::Spot,
                base: asset(&m.base_unit)?,
                quote: asset(&m.quote_unit)?,
                tick_size: Price::new(step(m.quote_unit_precision)?),
                lot_size: Qty::new(step(m.base_unit_precision)?),
                min_qty: Qty::new(raw_decimal(m.min_base_amount, "min_base_amount")?),
                min_notional: Notional::new(raw_decimal(m.min_quote_amount, "min_quote_amount")?),
                price_band: None,
                max_open_orders: None,
                max_open_stops: None,
            })
            .map_err(|e| decode(format!("market {}: {e}", m.id)))?;
            Ok(Market {
                instrument,
                status: m.status,
            })
        })
        .collect()
}

/// Parses `/api/v3/k`: rows of `[open time (s), open, high, low, close,
/// volume]`. MAX's REST klines carry no quote volume, so it is estimated as
/// volume × (open + high + low + close) / 4: inside the bar's true range,
/// exact when the price didn't move, otherwise an approximation (it only
/// matters to strategies reading quote volume, such as `avwap`).
pub fn parse_klines(
    body: &str,
    id: &InstrumentId,
    interval: Interval,
) -> Result<Vec<Candle>, ExchangeError> {
    let rows: Vec<Vec<&RawValue>> =
        serde_json::from_str(body).map_err(|e| decode(format!("klines: {e}")))?;
    rows.into_iter()
        .map(|r| {
            let [t, o, h, l, c, v] = r[..] else {
                return Err(decode("kline row: want 6 fields"));
            };
            let open_s = i64::from_str(t.get()).map_err(|_| decode("kline open time"))?;
            let open_time = open_s * 1000;
            let [o, h, l, c] = [o, h, l, c].map(|x| raw_decimal(x, "kline price"));
            let (o, h, l, c) = (o?, h?, l?, c?);
            let volume = raw_decimal(v, "kline volume")?;
            Candle::new(CandleSpec {
                instrument: id.clone(),
                interval,
                open_time: Timestamp::from_millis(open_time),
                close_time: Timestamp::from_millis(open_time + interval.as_millis() - 1),
                open: Price::new(o),
                high: Price::new(h),
                low: Price::new(l),
                close: Price::new(c),
                volume: Qty::new(volume),
                quote_volume: Notional::new(volume * (o + h + l + c) / Decimal::from(4)),
            })
            .map_err(|e| decode(e.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn max() -> VenueId {
        VenueId::new("max").unwrap()
    }

    /// Captured from `GET /api/v3/markets` on 2026-10-28 (four of 88).
    const MARKETS: &str = r#"[
      {"id":"btctwd","status":"active","base_unit":"btc","base_unit_precision":8,"min_base_amount":0.0001,"quote_unit":"twd","quote_unit_precision":1,"min_quote_amount":250.0,"m_wallet_supported":true},
      {"id":"btcusdt","status":"active","base_unit":"btc","base_unit_precision":6,"min_base_amount":0.0001,"quote_unit":"usdt","quote_unit_precision":2,"min_quote_amount":8.0,"m_wallet_supported":true},
      {"id":"dogeusdt","status":"active","base_unit":"doge","base_unit_precision":0,"min_base_amount":85.0,"quote_unit":"usdt","quote_unit_precision":4,"min_quote_amount":8.0,"m_wallet_supported":false},
      {"id":"maxusdt","status":"active","base_unit":"max","base_unit_precision":2,"min_base_amount":28.0,"quote_unit":"usdt","quote_unit_precision":4,"min_quote_amount":8.0,"m_wallet_supported":false}
    ]"#;

    #[test]
    fn precision_is_per_market_not_per_coin() {
        let ms = parse_markets(MARKETS, &max()).unwrap();
        let by = |s: &str| {
            ms.iter()
                .find(|m| m.instrument.id.symbol().as_str() == s)
                .map(|m| &m.instrument)
                .unwrap()
        };
        let twd = by("BTCTWD");
        assert_eq!(twd.id.to_string(), "max:BTCTWD");
        assert_eq!((twd.base.as_str(), twd.quote.as_str()), ("BTC", "TWD"));
        assert_eq!(twd.lot_size, Qty::new(dec!(0.00000001)));
        assert_eq!(twd.tick_size, Price::new(dec!(0.1)));
        assert_eq!(twd.min_notional, Notional::new(dec!(250)));
        // Same coin, another market: two fewer decimals.
        let usdt = by("BTCUSDT");
        assert_eq!(usdt.lot_size, Qty::new(dec!(0.000001)));
        assert_eq!(usdt.tick_size, Price::new(dec!(0.01)));
        assert_eq!(usdt.min_qty, Qty::new(dec!(0.0001)));
        assert_eq!(usdt.min_notional, Notional::new(dec!(8)));
        // Precision 0: whole coins only.
        let doge = by("DOGEUSDT");
        assert_eq!(doge.lot_size, Qty::new(dec!(1)));
        assert_eq!(doge.tick_size, Price::new(dec!(0.0001)));
        assert_eq!(doge.min_qty, Qty::new(dec!(85)));
        assert!(ms.iter().all(|m| m.status == "active"));
        assert!(ms.iter().all(|m| m.instrument.price_band.is_none()));
    }

    #[test]
    fn negative_precision_steps_by_powers_of_ten() {
        assert_eq!(step(8).unwrap(), dec!(0.00000001));
        assert_eq!(step(0).unwrap(), dec!(1));
        assert_eq!(step(-3).unwrap(), dec!(1000));
        assert!(step(19).is_err() && step(-19).is_err());
        let body = MARKETS.replace(
            r#""base_unit_precision":0,"min_base_amount":85.0"#,
            r#""base_unit_precision":-3,"min_base_amount":85000.0"#,
        );
        let ms = parse_markets(&body, &max()).unwrap();
        let doge = &ms[2].instrument;
        assert_eq!(doge.lot_size, Qty::new(dec!(1000)));
        assert_eq!(doge.round_qty(Qty::new(dec!(86999))), Qty::new(dec!(86000)));
    }

    /// Text parsed exactly: a float would turn 0.1 + 0.2 style values, and
    /// long fractions, into near misses.
    #[test]
    fn numbers_are_read_from_their_text() {
        let raw = |s: &str| RawValue::from_string(s.to_owned()).unwrap();
        assert_eq!(raw_decimal(&raw("0.0001"), "x").unwrap(), dec!(0.0001));
        assert_eq!(raw_decimal(&raw("28.0"), "x").unwrap(), dec!(28));
        assert_eq!(raw_decimal(&raw("1e-05"), "x").unwrap(), dec!(0.00001));
        assert_eq!(
            raw_decimal(&raw("0.30000000000000004441"), "x").unwrap(),
            dec!(0.30000000000000004441)
        );
        assert_eq!(
            raw_decimal(&raw(r#""81418.53""#), "x").unwrap(),
            dec!(81418.53)
        );
        assert!(raw_decimal(&raw("true"), "x").is_err());
        let bad = MARKETS.replace("\"min_quote_amount\":8.0", "\"min_quote_amount\":\"x\"");
        assert!(parse_markets(&bad, &max()).is_err());
    }

    /// Captured from `GET /api/v3/k?market=btcusdt&period=60` on 2026-10-28.
    #[test]
    fn parses_klines_with_estimated_quote_volume() {
        let body = "[[1793178000,81522.59,81522.59,81368.75,81514.66,0.8198],\
                    [1793181600,81536.46,81574.58,81479.93,81524.53,1.0454]]";
        let id: InstrumentId = "max:BTCUSDT".parse().unwrap();
        let cs = parse_klines(body, &id, Interval::H1).unwrap();
        assert_eq!(cs.len(), 2);
        let c = &cs[0];
        assert_eq!(c.open_time, Timestamp::from_millis(1_793_178_000_000));
        assert_eq!(c.close_time, Timestamp::from_millis(1_793_181_599_999));
        assert_eq!(
            (c.open, c.high, c.low, c.close),
            (
                Price::new(dec!(81522.59)),
                Price::new(dec!(81522.59)),
                Price::new(dec!(81368.75)),
                Price::new(dec!(81514.66))
            )
        );
        assert_eq!(c.volume, Qty::new(dec!(0.8198)));
        // 0.8198 × (81522.59 + 81522.59 + 81368.75 + 81514.66) / 4
        // = 0.8198 × 81482.1475 = 66799.0645205.
        assert_eq!(c.quote_volume, Notional::new(dec!(66799.0645205)));
        assert!(parse_klines("[[1,2,3]]", &id, Interval::H1).is_err());
        assert!(period_minutes(Interval::S1).is_err());
        assert_eq!(period_minutes(Interval::H4).unwrap(), 240);
    }
}
