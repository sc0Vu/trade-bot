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

/// Inverse of [`days_from_civil`]: (year, month, day) of a day number
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD HH:MM UTC`, for messages.
pub fn format(t: Timestamp) -> String {
    let ms = t.as_millis();
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let (h, min) = (rest / 3_600_000, rest % 3_600_000 / 60_000);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02} UTC")
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
    fn formats_what_it_parses() {
        assert_eq!(format(Timestamp::from_millis(0)), "1970-01-01 00:00 UTC");
        // 2024-02-20T03:00:00Z, checked with Python's datetime.
        assert_eq!(
            format(Timestamp::from_millis(1_708_398_000_000)),
            "2024-02-20 03:00 UTC"
        );
        for day in ["2000-02-29", "2024-12-31", "1999-03-01"] {
            assert_eq!(format(parse(day).unwrap()), format!("{day} 00:00 UTC"));
        }
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
