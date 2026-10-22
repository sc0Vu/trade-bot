use crate::{Error, InstrumentId, Notional, Price, Qty, Timestamp, VenueId};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

/// Bar width. A fixed set rather than free text, so a config typo errors
/// instead of silently naming a width no venue serves. Serde names match
/// Binance (`"1m"`, `"1h"`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Interval {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "1d")]
    D1,
}

impl Interval {
    /// Binance's code for the interval, also its serde name (`"1h"`).
    pub const fn code(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }

    /// Bar width in milliseconds.
    pub const fn as_millis(self) -> i64 {
        const MIN: i64 = 60_000;
        match self {
            Self::M1 => MIN,
            Self::M5 => 5 * MIN,
            Self::M15 => 15 * MIN,
            Self::H1 => 60 * MIN,
            Self::H4 => 240 * MIN,
            Self::D1 => 1_440 * MIN,
        }
    }
}

/// Unchecked bar data. Turn it into a [`Candle`] with [`Candle::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandleSpec {
    pub instrument: InstrumentId,
    pub interval: Interval,
    pub open_time: Timestamp,
    /// The bar's event time; the engine orders events by it.
    pub close_time: Timestamp,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    /// Traded volume in base units.
    pub volume: Qty,
    /// Traded volume in quote units; feeds `filters.min_24h_quote_volume`.
    pub quote_volume: Notional,
}

/// One CLOSED bar.
///
/// Providers emit a candle only after `close_time` has passed, so a strategy
/// never sees a still-forming bar (invariant 2). Always internally
/// consistent: `open_time < close_time`, `0 < low <= open, close <= high`,
/// volumes non-negative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CandleSpec", into = "CandleSpec")]
pub struct Candle(CandleSpec);

impl Candle {
    /// Validates the bar. A bad bar from a feed or cache is rejected here
    /// rather than reaching the fill model, where it would fill at a price
    /// that never traded.
    pub fn new(spec: CandleSpec) -> Result<Self, Error> {
        if spec.open_time >= spec.close_time {
            return Err(Error::InvalidCandle("open_time must precede close_time"));
        }
        if !spec.low.is_positive() {
            return Err(Error::InvalidCandle("prices must be positive"));
        }
        if spec.low > spec.open.min(spec.close) || spec.high < spec.open.max(spec.close) {
            return Err(Error::InvalidCandle(
                "open and close must lie within low..=high",
            ));
        }
        if spec.volume.is_negative() || spec.quote_volume.is_negative() {
            return Err(Error::InvalidCandle("volume must be non-negative"));
        }
        Ok(Self(spec))
    }
}

impl Deref for Candle {
    type Target = CandleSpec;
    fn deref(&self) -> &CandleSpec {
        &self.0
    }
}

impl TryFrom<CandleSpec> for Candle {
    type Error = Error;
    fn try_from(spec: CandleSpec) -> Result<Self, Error> {
        Self::new(spec)
    }
}

impl From<Candle> for CandleSpec {
    fn from(c: Candle) -> Self {
        c.0
    }
}

/// Unchecked top-of-book data. Turn it into a [`Quote`] with [`Quote::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteSpec {
    pub instrument: InstrumentId,
    pub ts: Timestamp,
    pub bid: Price,
    pub bid_qty: Qty,
    pub ask: Price,
    pub ask_qty: Qty,
}

/// Top of book. The type exists now; v1 never emits it. Always valid:
/// `0 < bid <= ask`, sizes non-negative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "QuoteSpec", into = "QuoteSpec")]
pub struct Quote(QuoteSpec);

impl Quote {
    /// Rejects a crossed book (`bid > ask`), non-positive prices, and
    /// negative sizes.
    pub fn new(spec: QuoteSpec) -> Result<Self, Error> {
        if !spec.bid.is_positive() {
            return Err(Error::InvalidQuote("prices must be positive"));
        }
        if spec.bid > spec.ask {
            return Err(Error::InvalidQuote("bid must not exceed ask"));
        }
        if spec.bid_qty.is_negative() || spec.ask_qty.is_negative() {
            return Err(Error::InvalidQuote("sizes must be non-negative"));
        }
        Ok(Self(spec))
    }
}

impl Deref for Quote {
    type Target = QuoteSpec;
    fn deref(&self) -> &QuoteSpec {
        &self.0
    }
}

impl TryFrom<QuoteSpec> for Quote {
    type Error = Error;
    fn try_from(spec: QuoteSpec) -> Result<Self, Error> {
        Self::new(spec)
    }
}

impl From<Quote> for QuoteSpec {
    fn from(q: Quote) -> Self {
        q.0
    }
}

/// Corporate action (split, dividend). Has no variants in v1, so a
/// `MarketEvent::CorporateAction` cannot be built until equities define one:
/// the compiler enforces "not emitted in v1", and adding variants later is
/// additive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CorpAction {}

/// Everything a strategy reacts to.
///
/// Backtest replays this exact stream, so backtest/live parity holds for
/// every variant, not only candles (invariant 3). v1 emits `Candle`, and a
/// live feed also `Backfill` (history only; see the variant).
/// Non-exhaustive so strategies in other crates keep a fallback arm and
/// survive new variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MarketEvent {
    Candle(Candle),
    /// A closed candle delivered late, for history only: the bars a live
    /// feed fetches to warm up after a start, or to fill the gap after a
    /// reconnect (except the newest, which comes as `Candle`). The engine
    /// adds it to the strategy's history; the default `on_event` doesn't
    /// decide on it, since its next open has long passed. A backtest never
    /// emits it: it has no outages.
    Backfill(Candle),
    Quote(Quote),
    /// Venue's trading session opened (equities). Carries its time so it
    /// orders against candles like any other event.
    SessionOpen {
        venue: VenueId,
        ts: Timestamp,
    },
    /// Venue's trading session closed (equities).
    SessionClose {
        venue: VenueId,
        ts: Timestamp,
    },
    CorporateAction(CorpAction),
    // Expiry(InstrumentId): deferred with options
}

impl MarketEvent {
    /// Event time. For a candle this is `close_time`: the moment the bar
    /// became known, never its open. The engine rejects a stream where this
    /// goes backwards.
    pub fn ts(&self) -> Timestamp {
        match self {
            Self::Candle(c) | Self::Backfill(c) => c.close_time,
            Self::Quote(q) => q.ts,
            Self::SessionOpen { ts, .. } | Self::SessionClose { ts, .. } => *ts,
            Self::CorporateAction(a) => match *a {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn spec() -> CandleSpec {
        CandleSpec {
            instrument: "binance:BTCUSDT".parse().unwrap(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(1_704_067_200_000),
            close_time: Timestamp::from_millis(1_704_070_799_999),
            open: Price::new(dec!(100)),
            high: Price::new(dec!(110)),
            low: Price::new(dec!(95)),
            close: Price::new(dec!(105)),
            volume: Qty::new(dec!(12.5)),
            quote_volume: Notional::new(dec!(1300)),
        }
    }

    #[test]
    fn valid_candle_reads_through() {
        let c = Candle::new(spec()).unwrap();
        assert_eq!(c.close, Price::new(dec!(105)));
    }

    #[test]
    fn rejects_inconsistent_bars() {
        let cases = [
            CandleSpec {
                close_time: spec().open_time,
                ..spec()
            },
            CandleSpec {
                high: Price::new(dec!(104)),
                ..spec()
            },
            CandleSpec {
                low: Price::new(dec!(101)),
                ..spec()
            },
            CandleSpec {
                low: Price::ZERO,
                open: Price::ZERO,
                ..spec()
            },
            CandleSpec {
                volume: Qty::new(dec!(-1)),
                ..spec()
            },
        ];
        for c in cases {
            assert!(Candle::new(c).is_err());
        }
    }

    #[test]
    fn candle_event_time_is_close_time() {
        let e = MarketEvent::Candle(Candle::new(spec()).unwrap());
        assert_eq!(e.ts(), Timestamp::from_millis(1_704_070_799_999));
    }

    #[test]
    fn session_event_time() {
        let e = MarketEvent::SessionOpen {
            venue: VenueId::new("alpaca").unwrap(),
            ts: Timestamp::from_millis(42),
        };
        assert_eq!(e.ts(), Timestamp::from_millis(42));
    }

    #[test]
    fn interval_code_matches_serde_name() {
        for i in [
            Interval::M1,
            Interval::M5,
            Interval::M15,
            Interval::H1,
            Interval::H4,
            Interval::D1,
        ] {
            assert_eq!(
                serde_json::to_string(&i).unwrap(),
                format!("\"{}\"", i.code())
            );
        }
    }

    #[test]
    fn interval_widths() {
        assert_eq!(Interval::M1.as_millis(), 60_000);
        assert_eq!(Interval::H1.as_millis(), 3_600_000);
        assert_eq!(Interval::D1.as_millis(), 86_400_000);
    }

    #[test]
    fn quote_rejects_crossed_book() {
        let q = QuoteSpec {
            instrument: "binance:BTCUSDT".parse().unwrap(),
            ts: Timestamp::from_millis(0),
            bid: Price::new(dec!(101)),
            bid_qty: Qty::new(dec!(1)),
            ask: Price::new(dec!(100)),
            ask_qty: Qty::new(dec!(1)),
        };
        assert!(Quote::new(q.clone()).is_err());
        assert!(
            Quote::new(QuoteSpec {
                ask: Price::new(dec!(101)),
                ..q
            })
            .is_ok()
        );
    }

    #[test]
    fn deserializing_a_bad_candle_fails() {
        let json = serde_json::to_string(&Candle::new(spec()).unwrap()).unwrap();
        let bad = json.replace(r#""high":"110""#, r#""high":"1""#);
        assert!(serde_json::from_str::<Candle>(&bad).is_err());
    }
}
