//! Bare symbols in configs and params: `AAPL.RTH` for `etoro:AAPL.RTH`
//! when the venue is already given (`exchange.venue`, `--venue`).
//!
//! - A whitelist entry without `venue:` takes the configured venue.
//! - In strategy params, any string, list item or table key that exactly
//!   names a symbol of the universe (case-insensitive) becomes its full id:
//!   `coin = "AAPL.RTH"`, `coins = ["QQQ", "MSFT"]`, `weights = { BTC =
//!   "0.4" }`. Nothing else is touched (`anchor = "day"`, a script path).
//! - A bare symbol that the universe has on two venues (a hedge's spot and
//!   perpetual `BTCUSDT`) is refused: write `venue:SYMBOL` there.
//!
//! Full ids stay valid everywhere, and everything downstream (books, logs,
//! the API, orders) sees full ids only.

use anyhow::{Context, bail};
use bot_core::{InstrumentId, Symbol, VenueId};

/// `entries` as ids: `venue:SYMBOL` as written, a bare symbol on `venue`.
pub(crate) fn qualify(entries: &[String], venue: &VenueId) -> anyhow::Result<Vec<InstrumentId>> {
    entries
        .iter()
        .map(|e| {
            if e.contains(':') {
                e.parse().with_context(|| format!("instrument {e:?}"))
            } else {
                Ok(InstrumentId::new(
                    venue.clone(),
                    Symbol::new(e).with_context(|| format!("symbol {e:?}"))?,
                ))
            }
        })
        .collect()
}

/// Rewrites the bare symbols in `params` that name an instrument of
/// `universe` to their full ids (module docs). The `script` key (a path)
/// is left alone.
pub(crate) fn qualify_params(
    params: &mut toml::Table,
    universe: &[InstrumentId],
) -> anyhow::Result<()> {
    let keys: Vec<String> = params.keys().cloned().collect();
    for key in keys {
        if key == "script" {
            continue;
        }
        if let Some(v) = params.get_mut(&key) {
            qualify_value(v, universe)?;
        }
    }
    Ok(())
}

/// The full id `s` stands for, if it is a bare symbol of the universe.
fn full(s: &str, universe: &[InstrumentId]) -> anyhow::Result<Option<String>> {
    if s.contains(':') || s.is_empty() {
        return Ok(None);
    }
    let matches: Vec<&InstrumentId> = universe
        .iter()
        .filter(|i| i.symbol().as_str().eq_ignore_ascii_case(s))
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.to_string())),
        many => bail!(
            "{s:?} is on more than one venue here ({}): write the full id",
            many.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn qualify_value(v: &mut toml::Value, universe: &[InstrumentId]) -> anyhow::Result<()> {
    match v {
        toml::Value::String(s) => {
            if let Some(id) = full(s, universe)? {
                *s = id;
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                qualify_value(item, universe)?;
            }
        }
        toml::Value::Table(t) => {
            let old = std::mem::take(t);
            for (k, mut item) in old {
                qualify_value(&mut item, universe)?;
                let k = full(&k, universe)?.unwrap_or(k);
                t.insert(k, item);
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<InstrumentId> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn bare_whitelist_entries_take_the_venue() {
        let venue: VenueId = "etoro".parse().unwrap();
        let got = qualify(
            &["AAPL.RTH".into(), "etoro:BTC".into(), "msft".into()],
            &venue,
        )
        .unwrap();
        assert_eq!(got, ids(&["etoro:AAPL.RTH", "etoro:BTC", "etoro:MSFT"]));
    }

    /// Strings, list items and table keys naming a universe symbol become
    /// full ids; other strings, numbers, full ids and the script path stay.
    #[test]
    fn params_naming_a_symbol_get_its_full_id() {
        let universe = ids(&["etoro:AAPL.RTH", "etoro:BTC", "etoro:QQQ"]);
        let mut p: toml::Table = r#"
            script = "strategies/BTC.rhai"
            coin = "aapl.rth"
            coins = ["QQQ", "etoro:BTC"]
            weights = { BTC = "0.4", "etoro:QQQ" = "0.2" }
            anchor = "day"
            lookback = 60
            fee = "0.001"
        "#
        .parse()
        .unwrap();
        qualify_params(&mut p, &universe).unwrap();
        let want: toml::Table = r#"
            script = "strategies/BTC.rhai"
            coin = "etoro:AAPL.RTH"
            coins = ["etoro:QQQ", "etoro:BTC"]
            weights = { "etoro:BTC" = "0.4", "etoro:QQQ" = "0.2" }
            anchor = "day"
            lookback = 60
            fee = "0.001"
        "#
        .parse()
        .unwrap();
        assert_eq!(p, want);
    }

    /// A hedge has BTCUSDT on two venues: bare is ambiguous, full is fine.
    #[test]
    fn a_symbol_on_two_venues_must_be_written_in_full() {
        let universe = ids(&["binance:BTCUSDT", "binance-usdm:BTCUSDT"]);
        let mut p: toml::Table = r#"spot = "BTCUSDT""#.parse().unwrap();
        let err = qualify_params(&mut p, &universe).unwrap_err().to_string();
        assert!(err.contains("more than one venue"), "{err}");
        let mut p: toml::Table = r#"spot = "binance:BTCUSDT"
               perp = "binance-usdm:BTCUSDT""#
            .parse()
            .unwrap();
        assert!(qualify_params(&mut p, &universe).is_ok());
    }
}
