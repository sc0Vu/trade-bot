//! `YYYY-MM-DD` (UTC midnight) to `Timestamp`, without a date crate.

use anyhow::{Context, bail};
use bot_core::Timestamp;

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses `YYYY-MM-DD` as UTC midnight.
pub fn parse(s: &str) -> anyhow::Result<Timestamp> {
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        bail!("date {s:?}: want YYYY-MM-DD");
    };
    let num = |p: &str| {
        p.parse::<i64>()
            .with_context(|| format!("date {s:?}: want YYYY-MM-DD"))
    };
    let (y, m, d) = (num(y)?, num(m)?, num(d)?);
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        bail!("date {s:?}: no such day");
    }
    Ok(Timestamp::from_millis(
        days_from_civil(y, m, d) * 86_400_000,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(parse("1970-01-01").unwrap().as_millis(), 0);
        // Binance's first 2024 hourly kline opens at 1704067200000.
        assert_eq!(parse("2024-01-01").unwrap().as_millis(), 1_704_067_200_000);
        assert_eq!(parse("2024-03-01").unwrap().as_millis(), 1_709_251_200_000);
    }

    #[test]
    fn rejects_bad_dates() {
        assert!(parse("2024-02-29").is_ok());
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "2024-1",
            "2024-01-00",
            "x-01-01",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
