use crate::{Params, Strategy, StrategyError};

/// One self-registered strategy. A strategy file adds itself with
/// `inventory::submit!`, so adding a strategy is adding a file: no central
/// match to edit.
///
/// ```ignore
/// inventory::submit! {
///     StrategyRegistration { name: "my_strategy", build: MyStrategy::build }
/// }
/// ```
pub struct StrategyRegistration {
    pub name: &'static str,
    /// Builds an instance from `strategy.params`; rejects bad params here,
    /// before trading starts.
    pub build: fn(&Params) -> Result<Box<dyn Strategy>, StrategyError>,
}

inventory::collect!(StrategyRegistration);

/// Builds the strategy registered as `name`.
pub fn build(name: &str, params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
    let mut found = inventory::iter::<StrategyRegistration>().filter(|r| r.name == name);
    let reg = found
        .next()
        .ok_or_else(|| StrategyError::Unknown(name.to_owned()))?;
    if found.next().is_some() {
        return Err(StrategyError::Duplicate(reg.name));
    }
    (reg.build)(params)
}

/// Every registered name, sorted.
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<_> = inventory::iter::<StrategyRegistration>()
        .map(|r| r.name)
        .collect();
    v.sort_unstable();
    v
}
