//! Calendar retention for catalogue metadata, never library membership.
use super::Period;

pub const DAY: u64 = 24 * 60 * 60;

/// Use the latest possible month for dates with only year precision.
pub fn month(date: &str) -> Option<i32> {
    let (_, high) = Period::release_period(date)?.bounds().ok()?;
    let high = high?;
    Some(high[..4].parse::<i32>().ok()? * 12 + high[5..7].parse::<i32>().ok()? - 1)
}

pub fn current_month(time: u64) -> Option<i32> {
    let seconds = i64::try_from(time).ok()?;
    let date = jiff::Timestamp::from_second(seconds)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::UTC);
    Some(i32::from(date.year()) * 12 + i32::from(date.month()) - 1)
}

pub fn fresh(stored_at: u64, latest_month: Option<i32>, time: u64) -> bool {
    let Some(age) = time.checked_sub(stored_at) else {
        return false;
    };
    let Some(current) = current_month(time) else {
        return false;
    };
    match latest_month {
        Some(month) if month < current - 1 => true,
        Some(month) if month == current - 1 => age < 7 * DAY,
        _ => age < DAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> u64 {
        value.parse::<jiff::Timestamp>().unwrap().as_second() as u64
    }

    #[test]
    fn retention_uses_calendar_months_and_exact_expiry_boundaries() {
        let now = at("2026-11-04T12:00:00Z");
        let old = month("2026-09-30");
        assert!(fresh(at("2020-01-01T00:00:00Z"), old, now));
        let previous = month("2026-10");
        assert!(fresh(now - 7 * DAY + 1, previous, now));
        assert!(!fresh(now - 7 * DAY, previous, now));
        for recent in [month("2026-11"), month("2026-12"), month("2026"), None] {
            assert!(fresh(now - DAY + 1, recent, now));
            assert!(!fresh(now - DAY, recent, now));
        }
        assert!(!fresh(now + 1, old, now));
    }

    #[test]
    fn january_rollover_and_imprecise_or_invalid_dates_are_conservative() {
        let now = at("2027-01-01T00:00:00Z");
        assert!(!fresh(now - 7 * DAY, month("2026-12"), now));
        assert!(fresh(now - 365 * DAY, month("2026-11"), now));
        assert_eq!(month("2026"), month("2026-12"));
        assert_eq!(month("2026-02-30"), None);
        assert_eq!(month(""), None);
    }
}
