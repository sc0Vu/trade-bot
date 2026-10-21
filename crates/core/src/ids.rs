//! String identifiers. Each is normalized to one case on construction so a
//! config typo in case (`btcusdt` vs `BTCUSDT`) can't silently miss the
//! whitelist, and each excludes `:` so `venue:SYMBOL` parses unambiguously.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Longest identifier accepted. Generous for crypto pairs and tickers; bounds
/// what a bad config or feed can push into keys and logs.
const MAX_ID_LEN: usize = 32;

fn valid_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $normalize:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates (1..=32 chars of `[A-Za-z0-9_.-]`) and normalizes case.
            pub fn new(value: impl AsRef<str>) -> Result<Self, Error> {
                let v = value.as_ref();
                if v.is_empty() || v.len() > MAX_ID_LEN || !v.chars().all(valid_id_char) {
                    return Err(Error::InvalidId { kind: $kind, value: v.to_owned() });
                }
                Ok(Self(v.$normalize()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self, Error> {
                Self::new(s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

string_id! {
    /// Venue key, lowercase (`binance`, `alpaca`).
    VenueId, "venue id", to_ascii_lowercase
}

string_id! {
    /// Venue's symbol for an instrument, uppercase (`BTCUSDT`, `BRK.B`).
    Symbol, "symbol", to_ascii_uppercase
}

string_id! {
    /// Asset or currency code, uppercase (`BTC`, `USDT`, `USD`).
    Asset, "asset", to_ascii_uppercase
}

/// Venue-qualified instrument key, written `venue:SYMBOL` (`binance:BTCUSDT`).
///
/// Carries the venue because one symbol on two venues is two instruments:
/// different fees, filters, and liquidity. Parses from and serializes as the
/// `venue:SYMBOL` string, so `config.whitelist` deserializes straight into it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstrumentId {
    venue: VenueId,
    symbol: Symbol,
}

impl InstrumentId {
    pub fn new(venue: VenueId, symbol: Symbol) -> Self {
        Self { venue, symbol }
    }

    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.venue, self.symbol)
    }
}

impl FromStr for InstrumentId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let (venue, symbol) = s.split_once(':').ok_or_else(|| Error::InvalidId {
            kind: "instrument id (want venue:SYMBOL)",
            value: s.to_owned(),
        })?;
        Ok(Self::new(venue.parse()?, symbol.parse()?))
    }
}

impl TryFrom<String> for InstrumentId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Error> {
        s.parse()
    }
}

impl From<InstrumentId> for String {
    fn from(id: InstrumentId) -> String {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_case() {
        let id: InstrumentId = "Binance:btcusdt".parse().unwrap();
        assert_eq!(id.venue().as_str(), "binance");
        assert_eq!(id.symbol().as_str(), "BTCUSDT");
        assert_eq!(id.to_string(), "binance:BTCUSDT");
    }

    #[test]
    fn case_variants_are_the_same_id() {
        let a: InstrumentId = "binance:BTCUSDT".parse().unwrap();
        let b: InstrumentId = "BINANCE:btcusdt".parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "BTCUSDT",
            ":BTCUSDT",
            "binance:",
            "binance:BTC:USDT",
            "binance:BTC USDT",
        ] {
            assert!(
                bad.parse::<InstrumentId>().is_err(),
                "{bad} should be rejected"
            );
        }
        assert!(Symbol::new("A".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn equity_ticker_with_dot_is_valid() {
        assert_eq!(Symbol::new("brk.b").unwrap().as_str(), "BRK.B");
    }

    #[derive(Deserialize)]
    struct Whitelist {
        whitelist: Vec<InstrumentId>,
    }

    #[test]
    fn whitelist_deserializes_from_toml() {
        let w: Whitelist =
            toml::from_str(r#"whitelist = ["binance:BTCUSDT", "binance:ethusdt"]"#).unwrap();
        assert_eq!(w.whitelist[1].to_string(), "binance:ETHUSDT");
        assert!(toml::from_str::<Whitelist>(r#"whitelist = ["BTCUSDT"]"#).is_err());
    }
}
