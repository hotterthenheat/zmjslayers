//! Civil-date arithmetic for day-count fractions.
//!
//! The engine pipeline needs a time-to-expiry in years from a snapshot
//! timestamp and an option expiry date. This is the only place the gateway
//! turns wall-clock-shaped data into a year fraction; the math kernel always
//! receives `t_years` as a pure parameter.

use slayer_core::{ExpiryDate, TsMillis};

/// Milliseconds per day.
const MS_PER_DAY: i64 = 86_400_000;
/// ACT/365 day-count basis.
const DAYS_PER_YEAR: f64 = 365.0;
/// Floor on the year fraction so a 0-DTE snapshot still prices (matches the
/// kernel's expectation of a positive tenor). One hour.
const MIN_T_YEARS: f64 = 1.0 / (DAYS_PER_YEAR * 24.0);

/// Days from the civil date 1970-01-01 (Howard Hinnant's algorithm), valid
/// for any proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Time to expiry in years (ACT/365) from `now` to end-of-day on `expiry`,
/// floored at [`MIN_T_YEARS`]. Expiry is treated as the market close of that
/// calendar day, approximated as the day boundary.
#[must_use]
pub fn years_to_expiry(now: TsMillis, expiry: ExpiryDate) -> f64 {
    let expiry_days = days_from_civil(i64::from(expiry.year), u32::from(expiry.month), u32::from(expiry.day));
    let expiry_ms = expiry_days * MS_PER_DAY;
    // now.0 is u64 epoch millis; expiry at day boundary.
    let delta_ms = expiry_ms - now.0 as i64;
    let years = delta_ms as f64 / (MS_PER_DAY as f64 * DAYS_PER_YEAR);
    years.max(MIN_T_YEARS)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_day_counts() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
    }

    #[test]
    fn one_year_out_is_about_one() {
        // 2026-01-01 to 2027-01-01 = 365 days.
        let now = TsMillis(days_from_civil(2026, 1, 1) as u64 * MS_PER_DAY as u64);
        let t = years_to_expiry(now, ExpiryDate { year: 2027, month: 1, day: 1 });
        assert!((t - 1.0).abs() < 1e-9, "got {t}");
    }

    #[test]
    fn past_expiry_floors() {
        let now = TsMillis(days_from_civil(2027, 1, 1) as u64 * MS_PER_DAY as u64);
        let t = years_to_expiry(now, ExpiryDate { year: 2026, month: 1, day: 1 });
        assert_eq!(t, MIN_T_YEARS);
    }
}
