use rust_decimal::Decimal;
use sea_orm::DeriveValueType;
use std::fmt;
use std::str::FromStr;

/// Exact decimal stored as TEXT. The one place money crosses into the
/// database; entities use it for every money column.
///
/// Parses with `from_str_exact`, so text that would need rounding (or a
/// value the backend coerced to a float) fails the read instead of
/// returning an approximation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, DeriveValueType)]
#[sea_orm(value_type = "String")]
pub(crate) struct DbDecimal(pub(crate) Decimal);

impl FromStr for DbDecimal {
    type Err = rust_decimal::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Decimal::from_str_exact(s).map(Self)
    }
}

impl fmt::Display for DbDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
