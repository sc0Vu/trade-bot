//! Keyless Binance REST: klines and instrument filters.
//!
//! Every price and quantity arrives as a JSON string and is parsed exactly
//! into the money newtypes (invariant 1); a number where a string is
//! expected is a decode error, never a float.

use super::signed::{HttpResponse, Method, admit, observe};
use crate::ExchangeError;
use crate::rate_limit::RateLimiter;
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Decimal, Instrument, InstrumentId, InstrumentSpec,
    Interval, Notional, Price, PriceBand, Qty, Symbol, Timestamp, VenueId,
};
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Binance's maximum klines per request.
const KLINE_LIMIT: usize = 1000;

/// Client for keyless Binance spot endpoints.
pub struct PublicClient {
    http: reqwest::Client,
    base: String,
    venue: VenueId,
    limiter: Arc<RateLimiter>,
}

impl PublicClient {
    /// `base` is the REST host, e.g. [`super::PUBLIC_DATA_URL`]. Instruments
    /// are tagged with venue `binance`. Requests time out after
    /// [`super::DEFAULT_HTTP_TIMEOUT`].
    pub fn new(base: impl Into<String>) -> Self {
        Self::with_timeout(base, super::DEFAULT_HTTP_TIMEOUT)
    }

    /// [`new`](Self::new) with a request timeout of `timeout`.
    pub fn with_timeout(base: impl Into<String>, timeout: std::time::Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // Fails only if TLS can't start; `Client::new` then panics too.
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            venue: VenueId::new("binance").unwrap_or_else(|_| unreachable!("valid venue id")),
            limiter: Arc::new(RateLimiter::new(super::limits::default_limits())),
        }
    }

    /// Paces requests through `limiter` instead of a private one (share it
    /// with every client of the same host).
    pub fn with_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.limiter = limiter;
        self
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ExchangeError> {
        let keys: Vec<String> = query.iter().map(|(k, _)| format!("{k}=")).collect();
        admit(&self.limiter, Method::Get, path, &keys.join("&")).await?;
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .send()
            .await
            .map_err(|e| ExchangeError::Http(e.to_string()))?;
        let status = res.status();
        let header = |name: &str| -> Option<u64> {
            res.headers().get(name)?.to_str().ok()?.trim().parse().ok()
        };
        let seen = HttpResponse {
            status: status.as_u16(),
            used_weight_1m: header("x-mbx-used-weight-1m"),
            order_count_10s: None,
            retry_after: header("retry-after"),
            body: String::new(),
        };
        observe(&self.limiter, &seen);
        let body = res
            .text()
            .await
            .map_err(|e| ExchangeError::Http(e.to_string()))?;
        if !status.is_success() {
            // 429/418 are rate-limit bans; surface them rather than retry blindly.
            return Err(ExchangeError::Http(format!(
                "{path}: HTTP {status}: {body}"
            )));
        }
        serde_json::from_str(&body).map_err(|e| ExchangeError::Decode(format!("{path}: {e}")))
    }

    /// Closed candles for `symbol` with `start <= open_time < end`, oldest
    /// first. Paginates by 1000. The still-forming candle (close time not
    /// yet passed) is never returned: a backtest must not see a bar the
    /// live engine wouldn't have (invariant 2).
    pub async fn klines(
        &self,
        symbol: &Symbol,
        interval: Interval,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let id = InstrumentId::new(self.venue.clone(), symbol.clone());
        let now = now_millis();
        let mut out = Vec::new();
        let mut cursor = start.as_millis();
        while cursor < end.as_millis() {
            let page = self
                .get(
                    "/api/v3/klines",
                    &[
                        ("symbol", symbol.to_string()),
                        ("interval", interval.code().to_owned()),
                        ("startTime", cursor.to_string()),
                        ("endTime", (end.as_millis() - 1).to_string()),
                        ("limit", KLINE_LIMIT.to_string()),
                    ],
                )
                .await?;
            let candles = parse_klines(&page, &id, interval)?;
            let n = candles.len();
            for c in candles {
                if c.open_time.as_millis() < cursor {
                    return Err(ExchangeError::Decode("klines went backwards".into()));
                }
                cursor = c.open_time.as_millis() + 1;
                if c.close_time.as_millis() < now && c.open_time < end {
                    out.push(c);
                }
            }
            if n < KLINE_LIMIT {
                break;
            }
        }
        Ok(out)
    }

    /// Current reference data (tick, lot, minimums) for one spot symbol.
    /// Filters are today's: Binance has changed tick sizes over time, so an
    /// old backtest uses current filters, an approximation.
    pub async fn instrument(&self, symbol: &Symbol) -> Result<Instrument, ExchangeError> {
        let info = self
            .get("/api/v3/exchangeInfo", &[("symbol", symbol.to_string())])
            .await?;
        super::limits::apply_published(&self.limiter, &info);
        parse_exchange_info(&info, &self.venue)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn decode(what: &str) -> ExchangeError {
    ExchangeError::Decode(what.to_owned())
}

fn str_field<T: FromStr>(v: &Value, what: &str) -> Result<T, ExchangeError> {
    v.as_str()
        .ok_or_else(|| decode(what))?
        .parse()
        .map_err(|_| decode(what))
}

fn int_field(v: &Value, what: &str) -> Result<i64, ExchangeError> {
    v.as_i64().ok_or_else(|| decode(what))
}

/// Parses a `/api/v3/klines` page: rows of
/// `[openTime, open, high, low, close, volume, closeTime, quoteVolume, ...]`.
pub(crate) fn parse_klines(
    page: &Value,
    id: &InstrumentId,
    interval: Interval,
) -> Result<Vec<Candle>, ExchangeError> {
    page.as_array()
        .ok_or_else(|| decode("klines: not an array"))?
        .iter()
        .map(|row| {
            let r = row
                .as_array()
                .filter(|r| r.len() >= 8)
                .ok_or_else(|| decode("kline row"))?;
            Candle::new(CandleSpec {
                instrument: id.clone(),
                interval,
                open_time: Timestamp::from_millis(int_field(&r[0], "kline open time")?),
                close_time: Timestamp::from_millis(int_field(&r[6], "kline close time")?),
                open: str_field::<Price>(&r[1], "kline open")?,
                high: str_field::<Price>(&r[2], "kline high")?,
                low: str_field::<Price>(&r[3], "kline low")?,
                close: str_field::<Price>(&r[4], "kline close")?,
                volume: str_field::<Qty>(&r[5], "kline volume")?,
                quote_volume: str_field::<Notional>(&r[7], "kline quote volume")?,
            })
            .map_err(|e| ExchangeError::Decode(e.to_string()))
        })
        .collect()
}

/// Parses `/api/v3/exchangeInfo?symbol=X` into an instrument.
pub(crate) fn parse_exchange_info(
    info: &Value,
    venue: &VenueId,
) -> Result<Instrument, ExchangeError> {
    let s = info["symbols"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| decode("exchangeInfo: no symbol"))?;
    let filter = |kind: &str| {
        s["filters"]
            .as_array()
            .and_then(|fs| fs.iter().find(|f| f["filterType"] == kind))
    };
    let price = filter("PRICE_FILTER").ok_or_else(|| decode("PRICE_FILTER"))?;
    let lot = filter("LOT_SIZE").ok_or_else(|| decode("LOT_SIZE"))?;
    // Newer symbols use NOTIONAL, older ones MIN_NOTIONAL.
    let notional = filter("NOTIONAL")
        .or_else(|| filter("MIN_NOTIONAL"))
        .ok_or_else(|| decode("NOTIONAL"))?;
    let symbol: Symbol = str_field(&s["symbol"], "symbol")?;
    let price_band = parse_price_band(filter("PERCENT_PRICE_BY_SIDE"), filter("PERCENT_PRICE"))?;
    Instrument::new(InstrumentSpec {
        id: InstrumentId::new(venue.clone(), symbol),
        class: AssetClass::Spot,
        base: Asset::new(s["baseAsset"].as_str().ok_or_else(|| decode("baseAsset"))?)
            .map_err(|e| ExchangeError::Decode(e.to_string()))?,
        quote: Asset::new(
            s["quoteAsset"]
                .as_str()
                .ok_or_else(|| decode("quoteAsset"))?,
        )
        .map_err(|e| ExchangeError::Decode(e.to_string()))?,
        tick_size: str_field(&price["tickSize"], "tickSize")?,
        lot_size: str_field(&lot["stepSize"], "stepSize")?,
        min_qty: str_field(&lot["minQty"], "minQty")?,
        min_notional: str_field(&notional["minNotional"], "minNotional")?,
        price_band,
        max_open_orders: None,
        max_open_stops: None,
    })
    .map_err(|e| ExchangeError::Decode(e.to_string()))
}

/// The price band from `PERCENT_PRICE_BY_SIDE`, or from the older
/// `PERCENT_PRICE` (one pair of multipliers for both sides). Neither: no
/// band. Present but malformed: an error, never a guess.
fn parse_price_band(
    by_side: Option<&Value>,
    legacy: Option<&Value>,
) -> Result<Option<PriceBand>, ExchangeError> {
    let mult = |f: &Value, key: &str| {
        f[key]
            .as_str()
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .ok_or_else(|| decode(key))
    };
    let minutes = |f: &Value| {
        f["avgPriceMins"]
            .as_u64()
            .and_then(|m| u32::try_from(m).ok())
            .ok_or_else(|| decode("avgPriceMins"))
    };
    Ok(match (by_side, legacy) {
        (Some(f), _) => Some(PriceBand {
            bid_up: mult(f, "bidMultiplierUp")?,
            bid_down: mult(f, "bidMultiplierDown")?,
            ask_up: mult(f, "askMultiplierUp")?,
            ask_down: mult(f, "askMultiplierDown")?,
            avg_minutes: minutes(f)?,
        }),
        (None, Some(f)) => {
            let (up, down) = (mult(f, "multiplierUp")?, mult(f, "multiplierDown")?);
            Some(PriceBand {
                bid_up: up,
                bid_down: down,
                ask_up: up,
                ask_down: down,
                avg_minutes: minutes(f)?,
            })
        }
        (None, None) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn btc() -> InstrumentId {
        "binance:BTCUSDT".parse().unwrap()
    }

    /// Shape copied from Binance's API docs.
    const KLINES: &str = r#"[
      [1704067200000, "42283.58000000", "42554.57000000", "42261.02000000", "42475.23000000",
       "1271.68108000", 1704070799999, "53957248.97378930", 47134, "682.57581000",
       "28957416.81909260", "0"]
    ]"#;

    #[test]
    fn parses_klines_exactly() {
        let page: Value = serde_json::from_str(KLINES).unwrap();
        let c = &parse_klines(&page, &btc(), Interval::H1).unwrap()[0];
        assert_eq!(c.open_time, Timestamp::from_millis(1_704_067_200_000));
        assert_eq!(c.close_time, Timestamp::from_millis(1_704_070_799_999));
        assert_eq!(c.close, Price::new(dec!(42475.23)));
        assert_eq!(c.quote_volume, Notional::new(dec!(53957248.97378930)));
    }

    #[test]
    fn numeric_price_is_a_decode_error() {
        let page: Value =
            serde_json::from_str(&KLINES.replace("\"42475.23000000\"", "42475.23")).unwrap();
        assert!(matches!(
            parse_klines(&page, &btc(), Interval::H1),
            Err(ExchangeError::Decode(_))
        ));
    }

    #[test]
    fn inconsistent_bar_is_a_decode_error() {
        // high below close
        let page: Value =
            serde_json::from_str(&KLINES.replace("42554.57000000", "1.00000000")).unwrap();
        assert!(parse_klines(&page, &btc(), Interval::H1).is_err());
    }

    const EXCHANGE_INFO: &str = r#"{"symbols":[{"symbol":"BTCUSDT","status":"TRADING",
      "baseAsset":"BTC","quoteAsset":"USDT","filters":[
        {"filterType":"PRICE_FILTER","minPrice":"0.01000000","maxPrice":"1000000.00000000","tickSize":"0.01000000"},
        {"filterType":"LOT_SIZE","minQty":"0.00001000","maxQty":"9000.00000000","stepSize":"0.00001000"},
        {"filterType":"NOTIONAL","minNotional":"5.00000000","applyMinToMarket":true}]}]}"#;

    /// BTCUSDT's band as Binance publishes it (mainnet and demo, Oct 2026).
    #[test]
    fn parses_price_band() {
        let with = EXCHANGE_INFO.replace(
            r#""applyMinToMarket":true}"#,
            r#""applyMinToMarket":true},
            {"filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"1.2","bidMultiplierDown":"0.5",
             "askMultiplierUp":"2","askMultiplierDown":"0.8","avgPriceMins":5}"#,
        );
        let venue = VenueId::new("binance").unwrap();
        let i = parse_exchange_info(&serde_json::from_str(&with).unwrap(), &venue).unwrap();
        assert_eq!(
            i.price_band,
            Some(PriceBand {
                bid_up: dec!(1.2),
                bid_down: dec!(0.5),
                ask_up: dec!(2),
                ask_down: dec!(0.8),
                avg_minutes: 5,
            })
        );
        let legacy = EXCHANGE_INFO.replace(
            r#""applyMinToMarket":true}"#,
            r#""applyMinToMarket":true},
            {"filterType":"PERCENT_PRICE","multiplierUp":"5","multiplierDown":"0.2","avgPriceMins":5}"#,
        );
        let i = parse_exchange_info(&serde_json::from_str(&legacy).unwrap(), &venue).unwrap();
        let b = i.price_band.unwrap();
        assert_eq!(
            (b.bid_up, b.ask_up, b.bid_down, b.ask_down),
            (dec!(5), dec!(5), dec!(0.2), dec!(0.2))
        );
        let none = parse_exchange_info(&serde_json::from_str(EXCHANGE_INFO).unwrap(), &venue);
        assert_eq!(none.unwrap().price_band, None);
        let float = with.replace(r#""bidMultiplierUp":"1.2""#, r#""bidMultiplierUp":1.2"#);
        assert!(parse_exchange_info(&serde_json::from_str(&float).unwrap(), &venue).is_err());
    }

    #[test]
    fn parses_instrument_filters() {
        let info: Value = serde_json::from_str(EXCHANGE_INFO).unwrap();
        let i = parse_exchange_info(&info, &VenueId::new("binance").unwrap()).unwrap();
        assert_eq!(i.id, btc());
        assert_eq!(i.tick_size, Price::new(dec!(0.01)));
        assert_eq!(i.lot_size, Qty::new(dec!(0.00001)));
        assert_eq!(i.min_notional, Notional::new(dec!(5)));
    }
}
