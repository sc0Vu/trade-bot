//! Money newtypes over `Decimal` (invariant 1: never a float).
//!
//! Three units, kept apart by the type system:
//! - [`Price`]: quote currency per one base unit (USDT per BTC).
//! - [`Qty`]: amount of an asset, base units of the instrument unless paired
//!   with an explicit `Asset`.
//! - [`Notional`]: amount of quote currency (order value, cash, P&L).
//!
//! Each type is closed under `+`, `-`, negation and scaling by a unitless
//! `Decimal`; `T / T` gives a unitless `Decimal` ratio. `Qty * Price` and
//! `Notional / Price` are deliberately not implemented: converting between
//! units goes through `Instrument::notional` / `Instrument::qty_for`
//! (invariant 4), so a later contract multiplier can't be bypassed.
//!
//! Serde reads and writes these only as strings (`"0.001"`). A TOML or JSON
//! number is rejected, because parsers turn it into a binary float before
//! `Decimal` ever sees it.
//!
//! Arithmetic follows `Decimal`: it panics on overflow (beyond ~7.9e28) and
//! on division by zero.

use crate::Error;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};
use std::str::FromStr;

macro_rules! decimal_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Decimal);

        impl $name {
            pub const ZERO: Self = Self(Decimal::ZERO);

            /// Wraps a raw decimal. The only way in; there is no `From<f64>`.
            pub const fn new(value: Decimal) -> Self {
                Self(value)
            }

            /// The raw decimal, for unitless math (ratios, statistics) and the
            /// venue wire format.
            pub const fn get(self) -> Decimal {
                self.0
            }

            /// Strictly greater than zero.
            pub fn is_positive(self) -> bool {
                self.0 > Decimal::ZERO
            }

            /// Strictly less than zero.
            pub fn is_negative(self) -> bool {
                self.0 < Decimal::ZERO
            }

            pub fn is_zero(self) -> bool {
                self.0.is_zero()
            }

            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }
        }

        impl Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }

        impl Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                self.0 -= rhs.0;
            }
        }

        impl Neg for $name {
            type Output = Self;
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }

        /// Scale by a unitless factor (fee rate, slippage, fraction).
        impl Mul<Decimal> for $name {
            type Output = Self;
            fn mul(self, rhs: Decimal) -> Self {
                Self(self.0 * rhs)
            }
        }

        /// Scale by a unitless divisor.
        impl Div<Decimal> for $name {
            type Output = Self;
            fn div(self, rhs: Decimal) -> Self {
                Self(self.0 / rhs)
            }
        }

        /// Same-unit ratio; the unit cancels.
        impl Div for $name {
            type Output = Decimal;
            fn div(self, rhs: Self) -> Decimal {
                self.0 / rhs.0
            }
        }

        impl Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, Add::add)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        /// Exact parse: rejects exponents and values that would need rounding.
        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                Decimal::from_str_exact(s)
                    .map(Self)
                    .map_err(|_| Error::InvalidDecimal(s.to_owned()))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

decimal_newtype! {
    /// Quote currency per one base unit (USDT per BTC, USD per share).
    Price
}

decimal_newtype! {
    /// Amount of an asset. Base units of the instrument (BTC, shares) unless
    /// paired with an explicit `Asset`, as in `Fee::Other`. Signed: negative
    /// means a short or a reduction; constructors of `Order` and `Fill`
    /// require it positive where the venue does.
    Qty
}

decimal_newtype! {
    /// Amount of quote currency: order value, cash balance, quote-side fee,
    /// P&L. Signed: negative is a loss or an outflow.
    Notional
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn decimal_sums_exactly_where_float_drifts() {
        // 0.1 + 0.2 is 0.30000000000000004 in f64; the newtype must give 0.3.
        let sum = Notional::new(dec!(0.1)) + Notional::new(dec!(0.2));
        assert_eq!(sum, Notional::new(dec!(0.3)));
    }

    #[test]
    fn ten_thousand_cent_fees_sum_to_exactly_one_hundred() {
        let total: Notional = (0..10_000).map(|_| Notional::new(dec!(0.01))).sum();
        assert_eq!(total, Notional::new(dec!(100)));
    }

    #[test]
    fn same_unit_ratio_is_unitless() {
        let r = Price::new(dec!(60000)) / Price::new(dec!(40000));
        assert_eq!(r, dec!(1.5));
    }

    #[test]
    fn scaling_by_fee_rate() {
        // 0.1% taker fee on 250 USDT is 0.25 USDT.
        let fee = Notional::new(dec!(250)) * dec!(0.001);
        assert_eq!(fee, Notional::new(dec!(0.25)));
    }

    #[test]
    fn sign_helpers_treat_zero_as_neither() {
        assert!(!Qty::ZERO.is_positive());
        assert!(!Qty::ZERO.is_negative());
        assert!(Qty::new(dec!(-1)).is_negative());
        assert_eq!(Qty::new(dec!(-1)).abs(), Qty::new(dec!(1)));
    }

    #[test]
    fn parse_is_exact() {
        assert_eq!("0.00001".parse::<Qty>(), Ok(Qty::new(dec!(0.00001))));
        assert!("1e-5".parse::<Qty>().is_err());
        assert!("abc".parse::<Qty>().is_err());
    }

    #[test]
    fn json_round_trips_as_string() {
        let p = Price::new(dec!(63123.45));
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#""63123.45""#);
        assert_eq!(serde_json::from_str::<Price>(&json).unwrap(), p);
    }

    #[test]
    fn json_number_is_rejected() {
        assert!(serde_json::from_str::<Price>("63123.45").is_err());
    }

    #[derive(Deserialize)]
    struct FeeConfig {
        fee: Notional,
    }

    #[test]
    fn toml_float_is_rejected_string_accepted() {
        assert!(toml::from_str::<FeeConfig>("fee = 0.001").is_err());
        let ok: FeeConfig = toml::from_str(r#"fee = "0.001""#).unwrap();
        assert_eq!(ok.fee, Notional::new(dec!(0.001)));
    }
}
