//! Binance USDⓈ-M futures (linear perpetuals): reference data, funding
//! history and margin brackets.
//!
//! Instruments are venue [`VENUE`] (`binance-usdm:BTCUSDT`), so a perpetual
//! never shares an id, cache file or books with the spot pair of the same
//! symbol. Klines come from [`PublicClient::usdm`] (same shape as spot).
//!
//! - **Instrument** (`GET /fapi/v1/exchangeInfo`, weight 1, all symbols):
//!   only `contractType = PERPETUAL` with `marginAsset = quoteAsset`
//!   (linear). Tick from `PRICE_FILTER`, lot / min qty from `LOT_SIZE`,
//!   minimum notional from `MIN_NOTIONAL.notional`, open-order caps from
//!   `MAX_NUM_ORDERS` / `MAX_NUM_ALGO_ORDERS` (`limit`). No price band:
//!   futures' `PERCENT_PRICE` is relative to the mark price, which the
//!   spot band check (trade average) doesn't model. `liquidationFee` = the
//!   symbol's clearance fee rate.
//! - **Funding** (`GET /fapi/v1/fundingRate`, ≤ 1000 per page): rate per
//!   settlement, stamped a few ms after the hour; `markPrice` is empty in
//!   older records (checked 2026-11-17: 2020 and 2022 empty) → `None`.
//! - **Brackets** (`GET /fapi/v1/leverageBracket`, signed, weight 1): money
//!   as JSON numbers → read from the text, never `f64` (invariant 1).
//!   Works with a demo key on the demo and testnet hosts (2026-11-17).

use super::public::{PublicClient, cap, str_field};
use super::signed::{Method, SignedClient};
use crate::ExchangeError;
use bot_core::margin::{Bracket, Brackets};
use bot_core::{
    Asset, AssetClass, Decimal, Instrument, InstrumentId, InstrumentSpec, Notional, Price, Symbol,
    Timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// Venue id of USDⓈ-M perpetual instruments.
pub const VENUE: &str = "binance-usdm";
/// Production REST host.
pub const LIVE_URL: &str = "https://fapi.binance.com";
/// Demo Trading futures host (fake money, demo keys).
pub const DEMO_URL: &str = "https://demo-fapi.binance.com";
/// Futures testnet host.
pub const TESTNET_URL: &str = "https://testnet.binancefuture.com";

/// Binance's maximum funding records per request.
const FUNDING_LIMIT: usize = 1000;

/// A perpetual's reference data plus what only futures publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerpInfo {
    pub instrument: Instrument,
    /// Liquidation clearance fee rate (`liquidationFee`, `0.0125`).
    pub liquidation_fee: Decimal,
}

/// One funding settlement from the venue's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingRate {
    /// `fundingTime`.
    pub ts: Timestamp,
    /// As a fraction (`0.0001`); positive: longs pay shorts.
    #[serde(with = "bot_core::decimal_str")]
    pub rate: Decimal,
    /// The mark price it was valued at; `None` where the venue left it
    /// empty (older history).
    pub mark: Option<Price>,
}

impl PublicClient {
    /// Today's reference data for perpetual `symbol` (module docs).
    pub async fn perp_instrument(&self, symbol: &Symbol) -> Result<PerpInfo, ExchangeError> {
        let info = self.get("/fapi/v1/exchangeInfo", &[]).await?;
        super::limits::apply_published(self.limiter(), &info);
        parse_perp_info(&info, symbol, self.venue())
    }

    /// Funding settlements of `symbol` with `start <= ts < end`, oldest
    /// first, paginated by 1000.
    pub async fn funding_rates(
        &self,
        symbol: &Symbol,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Vec<FundingRate>, ExchangeError> {
        let mut out = Vec::new();
        let mut cursor = start.as_millis();
        while cursor < end.as_millis() {
            let page = self
                .get(
                    "/fapi/v1/fundingRate",
                    &[
                        ("symbol", symbol.to_string()),
                        ("startTime", cursor.to_string()),
                        ("endTime", (end.as_millis() - 1).to_string()),
                        ("limit", FUNDING_LIMIT.to_string()),
                    ],
                )
                .await?;
            let rates = parse_funding(&page)?;
            let n = rates.len();
            for r in rates {
                if r.ts.as_millis() < cursor {
                    return Err(ExchangeError::Decode("funding went backwards".into()));
                }
                cursor = r.ts.as_millis() + 1;
                if r.ts < end {
                    out.push(r);
                }
            }
            if n < FUNDING_LIMIT {
                break;
            }
        }
        Ok(out)
    }
}

/// The account's margin brackets for `symbol` (signed).
pub async fn brackets(client: &SignedClient, symbol: &Symbol) -> Result<Brackets, ExchangeError> {
    let text = client
        .signed_text(
            Method::Get,
            "/fapi/v1/leverageBracket",
            &[("symbol", symbol.to_string())],
        )
        .await?;
    parse_brackets(&text, symbol)
}

fn decode(what: impl Into<String>) -> ExchangeError {
    ExchangeError::Decode(what.into())
}

/// `exchangeInfo` (all symbols) → `symbol`'s [`PerpInfo`].
pub(crate) fn parse_perp_info(
    info: &Value,
    symbol: &Symbol,
    venue: &bot_core::VenueId,
) -> Result<PerpInfo, ExchangeError> {
    let s = info["symbols"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["symbol"] == symbol.as_str()))
        .ok_or_else(|| {
            ExchangeError::UnknownInstrument(InstrumentId::new(venue.clone(), symbol.clone()))
        })?;
    if s["contractType"] != "PERPETUAL" {
        return Err(decode(format!("{symbol}: not a perpetual")));
    }
    if s["marginAsset"] != s["quoteAsset"] {
        return Err(decode(format!(
            "{symbol}: not linear (margin asset ≠ quote)"
        )));
    }
    let filter = |kind: &str| {
        s["filters"]
            .as_array()
            .and_then(|fs| fs.iter().find(|f| f["filterType"] == kind))
    };
    let price = filter("PRICE_FILTER").ok_or_else(|| decode("PRICE_FILTER"))?;
    let lot = filter("LOT_SIZE").ok_or_else(|| decode("LOT_SIZE"))?;
    let notional = filter("MIN_NOTIONAL").ok_or_else(|| decode("MIN_NOTIONAL"))?;
    let asset = |key: &str| {
        Asset::new(s[key].as_str().ok_or_else(|| decode(key))?)
            .map_err(|e| ExchangeError::Decode(e.to_string()))
    };
    let instrument = Instrument::new(InstrumentSpec {
        id: InstrumentId::new(venue.clone(), symbol.clone()),
        class: AssetClass::Perpetual,
        base: asset("baseAsset")?,
        quote: asset("quoteAsset")?,
        tick_size: str_field(&price["tickSize"], "tickSize")?,
        lot_size: str_field(&lot["stepSize"], "stepSize")?,
        min_qty: str_field(&lot["minQty"], "minQty")?,
        min_notional: str_field(&notional["notional"], "notional")?,
        price_band: None,
        max_open_orders: cap(filter("MAX_NUM_ORDERS"), "limit")?,
        max_open_stops: cap(filter("MAX_NUM_ALGO_ORDERS"), "limit")?,
    })
    .map_err(|e| ExchangeError::Decode(e.to_string()))?;
    let liquidation_fee: Decimal = str_field(&s["liquidationFee"], "liquidationFee")?;
    if liquidation_fee < Decimal::ZERO || liquidation_fee >= Decimal::ONE {
        return Err(decode("liquidationFee out of range"));
    }
    Ok(PerpInfo {
        instrument,
        liquidation_fee,
    })
}

/// A `fundingRate` page.
pub(crate) fn parse_funding(page: &Value) -> Result<Vec<FundingRate>, ExchangeError> {
    page.as_array()
        .ok_or_else(|| decode("fundingRate: not an array"))?
        .iter()
        .map(|r| {
            let mark = match r["markPrice"].as_str() {
                None | Some("") => None,
                Some(_) => Some(str_field::<Price>(&r["markPrice"], "markPrice")?),
            };
            Ok(FundingRate {
                ts: Timestamp::from_millis(
                    r["fundingTime"]
                        .as_i64()
                        .ok_or_else(|| decode("fundingTime"))?,
                ),
                rate: str_field(&r["fundingRate"], "fundingRate")?,
                mark,
            })
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSymbolBrackets<'a> {
    symbol: String,
    #[serde(borrow)]
    brackets: Vec<RawBracket<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBracket<'a> {
    initial_leverage: u32,
    #[serde(borrow)]
    notional_cap: &'a RawValue,
    #[serde(borrow)]
    notional_floor: &'a RawValue,
    #[serde(borrow)]
    maint_margin_ratio: &'a RawValue,
    #[serde(borrow)]
    cum: &'a RawValue,
}

/// A `leverageBracket` reply (array, or one object) → `symbol`'s brackets.
pub(crate) fn parse_brackets(text: &str, symbol: &Symbol) -> Result<Brackets, ExchangeError> {
    let all: Vec<RawSymbolBrackets> = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            vec![serde_json::from_str(text).map_err(|e| decode(format!("leverageBracket: {e}")))?]
        }
    };
    let one = all
        .into_iter()
        .find(|s| s.symbol == symbol.as_str())
        .ok_or_else(|| decode(format!("leverageBracket: no {symbol}")))?;
    let num = |r: &RawValue, what: &str| crate::max::public::raw_decimal(r, what);
    let brackets = one
        .brackets
        .iter()
        .map(|b| {
            Ok(Bracket {
                floor: Notional::new(num(b.notional_floor, "notionalFloor")?),
                cap: Notional::new(num(b.notional_cap, "notionalCap")?),
                max_leverage: b.initial_leverage,
                rate: num(b.maint_margin_ratio, "maintMarginRatio")?,
                cum: num(b.cum, "cum")?,
            })
        })
        .collect::<Result<Vec<_>, ExchangeError>>()?;
    Brackets::new(brackets).map_err(|e| ExchangeError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn venue() -> bot_core::VenueId {
        bot_core::VenueId::new(VENUE).unwrap()
    }

    /// BTCUSDT as `/fapi/v1/exchangeInfo` returned it on 2026-11-17
    /// (fields trimmed), next to a quarterly contract that must be skipped.
    const INFO: &str = r#"{"symbols":[
      {"symbol":"BTCUSDT_261225","contractType":"CURRENT_QUARTER","marginAsset":"USDT","quoteAsset":"USDT"},
      {"symbol":"BTCUSDT","contractType":"PERPETUAL","baseAsset":"BTC","quoteAsset":"USDT","marginAsset":"USDT","liquidationFee":"0.012500",
       "filters":[{"tickSize":"0.10","maxPrice":"4529764","minPrice":"556.80","filterType":"PRICE_FILTER"},
                  {"stepSize":"0.001","minQty":"0.001","filterType":"LOT_SIZE","maxQty":"1000"},
                  {"filterType":"MAX_NUM_ORDERS","limit":200},
                  {"notional":"50","filterType":"MIN_NOTIONAL"},
                  {"filterType":"PERCENT_PRICE","multiplierUp":"1.0300","multiplierDecimal":"4","multiplierDown":"0.9700"}]}]}"#;

    #[test]
    fn reads_a_linear_perpetual_from_exchange_info() {
        let info: Value = serde_json::from_str(INFO).unwrap();
        let p = parse_perp_info(&info, &"BTCUSDT".parse().unwrap(), &venue()).unwrap();
        let i = &p.instrument;
        assert_eq!(i.id.to_string(), "binance-usdm:BTCUSDT");
        assert_eq!(i.class, AssetClass::Perpetual);
        assert_eq!(i.tick_size.get(), dec!(0.1));
        assert_eq!(
            (i.lot_size.get(), i.min_qty.get()),
            (dec!(0.001), dec!(0.001))
        );
        assert_eq!(i.min_notional.get(), dec!(50));
        assert_eq!(
            (i.max_open_orders, i.max_open_stops, i.price_band),
            (Some(200), None, None)
        );
        assert_eq!(p.liquidation_fee, dec!(0.0125));
        let q = parse_perp_info(&info, &"BTCUSDT_261225".parse().unwrap(), &venue());
        assert!(q.is_err());
        assert!(parse_perp_info(&info, &"ETHUSDT".parse().unwrap(), &venue()).is_err());
    }

    /// Real records: 2020's carry an empty mark, 2025's a mark.
    #[test]
    fn funding_marks_may_be_missing() {
        let page: Value = serde_json::from_str(r#"[
          {"symbol":"BTCUSDT","fundingTime":1577836800000,"fundingRate":"-0.00012359","markPrice":"","rateType":"Regular"},
          {"symbol":"BTCUSDT","fundingTime":1735689600015,"fundingRate":"0.00010000","markPrice":"93549.66175177","rateType":"Regular"}]"#).unwrap();
        let r = parse_funding(&page).unwrap();
        assert_eq!(
            r[0],
            FundingRate {
                ts: Timestamp::from_millis(1577836800000),
                rate: dec!(-0.00012359),
                mark: None
            }
        );
        assert_eq!(r[1].mark, Some(Price::new(dec!(93549.66175177))));
        assert_eq!(r[1].rate, dec!(0.0001));
    }

    /// The first three BTCUSDT brackets as the demo host returned them on
    /// 2026-11-17: numbers, read exactly (0.004, not 0.004000000000000001).
    #[test]
    fn brackets_are_read_from_the_text() {
        let text = r#"[{"symbol":"BTCUSDT","brackets":[
          {"bracket":1,"initialLeverage":125,"notionalCap":50000,"notionalFloor":0,"maintMarginRatio":0.004,"cum":0.0},
          {"bracket":2,"initialLeverage":100,"notionalCap":250000,"notionalFloor":50000,"maintMarginRatio":0.005,"cum":50.0},
          {"bracket":3,"initialLeverage":50,"notionalCap":3000000,"notionalFloor":250000,"maintMarginRatio":0.01,"cum":1300.0}]}]"#;
        let b = parse_brackets(text, &"BTCUSDT".parse().unwrap()).unwrap();
        assert_eq!(b.max_leverage(Notional::new(dec!(49999))), 125);
        assert_eq!(b.max_leverage(Notional::new(dec!(250000))), 50);
        let second = b.iter().nth(1).unwrap();
        assert_eq!((second.rate, second.cum), (dec!(0.005), dec!(50)));
        // 100,000 notional: 100000 × 0.005 − 50 = 450.
        assert_eq!(
            b.maintenance(Notional::new(dec!(100000))),
            Notional::new(dec!(450))
        );
        assert!(parse_brackets(text, &"ETHUSDT".parse().unwrap()).is_err());
    }
}
