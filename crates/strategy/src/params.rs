use crate::StrategyError;
use serde::de::DeserializeOwned;

/// `strategy.params` from config: an untyped table that each strategy
/// deserializes into its own typed struct, so params are checked once, at
/// build time, never looked up by string while trading. Money values are
/// strings (`quote_amount = "100"`), like everywhere else in config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params(toml::Table);

impl Params {
    pub fn new(table: toml::Table) -> Self {
        Self(table)
    }

    /// Parses a TOML snippet (handy for tests and examples).
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        s.parse().map(Self)
    }

    /// Deserializes into `T`; errors name the strategy for the user.
    pub fn parse<T: DeserializeOwned>(&self, strategy: &str) -> Result<T, StrategyError> {
        toml::Value::Table(self.0.clone())
            .try_into()
            .map_err(|e: toml::de::Error| StrategyError::Params {
                strategy: strategy.to_owned(),
                reason: e.message().to_owned(),
            })
    }
}

/// Serde helper for strategy params: an exact decimal written as a string
/// (`fee = "0.001"`). A TOML number is rejected, since it would pass
/// through a float first (invariant 1).
pub fn exact_decimal<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<bot_core::Decimal, D::Error> {
    let s = <String as serde::Deserialize>::deserialize(d)?;
    bot_core::Decimal::from_str_exact(&s).map_err(serde::de::Error::custom)
}
