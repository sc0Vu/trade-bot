//! UTC calendar periods for strategies that act per hour, day, week or
//! month.

use bot_core::Timestamp;
use serde::Deserialize;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// A UTC calendar period; weeks start on Monday.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Period {
    Hour,
    Day,
    Week,
    Month,
}

/// Days since 1970-01-01 of the first day of `days`' month (proleptic
/// Gregorian, H. Hinnant's `civil_from_days` / `days_from_civil`).
fn month_start(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // Back to the first day of that month: same year-of-era and month.
    let doy1 = (153 * mp + 2) / 5;
    let doe1 = yoe * 365 + yoe / 4 - yoe / 100 + doy1;
    era * 146_097 + doe1 - 719_468
}

/// Start (UTC) of the hour, day, week (from Monday) or month `t` falls in.
pub(crate) fn period_start(period: Period, t: Timestamp) -> Timestamp {
    let ms = t.as_millis();
    let days = ms.div_euclid(DAY_MS);
    Timestamp::from_millis(match period {
        Period::Hour => ms.div_euclid(HOUR_MS) * HOUR_MS,
        Period::Day => days * DAY_MS,
        // Day 0 (1970-01-01) was a Thursday, 3 days after a Monday.
        Period::Week => (days - (days + 3).rem_euclid(7)) * DAY_MS,
        Period::Month => month_start(days) * DAY_MS,
    })
}

/// Day of the month (1-based) of `t`, and how many days its month has.
pub(crate) fn day_of_month(t: Timestamp) -> (i64, i64) {
    let start = period_start(Period::Month, t).as_millis();
    // 32 days past the first is always in the next month.
    let next = period_start(Period::Month, Timestamp::from_millis(start + 32 * DAY_MS)).as_millis();
    (
        (t.as_millis() - start).div_euclid(DAY_MS) + 1,
        (next - start) / DAY_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-02-29T10:00: day 29 of a 29-day February (leap year);
    /// 2023-02-28T23:59:59.999: day 28 of 28; 1970-01-31: day 31 of 31;
    /// 2024-04-30T00:00: day 30 of 30.
    #[test]
    fn days_of_month_count_leap_years() {
        let d = |ms| day_of_month(Timestamp::from_millis(ms));
        assert_eq!(d(1_709_200_800_000), (29, 29));
        assert_eq!(d(1_677_628_799_999), (28, 28));
        assert_eq!(d(30 * DAY_MS), (31, 31));
        assert_eq!(d(1_714_435_200_000), (30, 30));
    }

    /// Reference instants from Python's datetime (UTC).
    #[test]
    fn periods_start_on_utc_day_monday_and_first_of_month() {
        let start = |p, ms| period_start(p, Timestamp::from_millis(ms)).as_millis();
        // 2024-02-29T10:00 (leap day): day 2024-02-29, month 2024-02-01.
        assert_eq!(start(Period::Day, 1_709_200_800_000), 1_709_164_800_000);
        // 10:59:59.999 is in the 10:00 hour; 11:00 starts the next.
        assert_eq!(start(Period::Hour, 1_709_204_399_999), 1_709_200_800_000);
        assert_eq!(start(Period::Hour, 1_709_204_400_000), 1_709_204_400_000);
        assert_eq!(start(Period::Month, 1_709_200_800_000), 1_706_745_600_000);
        // 2024-03-01T00:00 starts its own month; 2023-12-31T23:00 is in
        // December's.
        assert_eq!(start(Period::Month, 1_709_251_200_000), 1_709_251_200_000);
        assert_eq!(start(Period::Month, 1_704_063_600_000), 1_701_388_800_000);
        // Monday 1970-01-05 (345600000) starts the week of Thursday 01-08
        // and Sunday 01-11; Monday 01-12 starts the next.
        assert_eq!(start(Period::Week, 648_000_000), 345_600_000);
        assert_eq!(start(Period::Week, 946_800_000), 345_600_000);
        assert_eq!(start(Period::Week, 950_400_000), 950_400_000);
        // Wednesday 2026-10-28T05:00 is in the week of Monday 2026-10-26.
        assert_eq!(start(Period::Week, 1_793_163_600_000), 1_792_972_800_000);
    }
}
