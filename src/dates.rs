//! Dates and lengths of time as admins give them and lurker reports them.
//! Days are counted in UTC.

use chrono::{DateTime, Days, NaiveDate};

/// The longest ago a number of days such as `348d` may reach, about 100
/// years, to catch typos.
const MAX_DAYS_AGO: u64 = 36_500;

/// The longest grace period, a year, to catch typos.
const MAX_GRACE: i64 = 365 * DAY;

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// The date in UTC at a time in seconds since 1970.
pub fn today(now: i64) -> NaiveDate {
    DateTime::from_timestamp(now, 0).map_or(NaiveDate::MIN, |time| time.date_naive())
}

/// A date given as `YYYY-MM-DD`, or as a number of days ago such as `348d`,
/// which is turned into a date so the count keeps growing.
pub fn parse(input: &str, today: NaiveDate) -> Result<NaiveDate, String> {
    let date = if let Some(number) = input.strip_suffix(['d', 'D']) {
        let days: u64 = number
            .parse()
            .ok()
            .filter(|&days| days <= MAX_DAYS_AGO)
            .ok_or_else(|| format!("{input} isn't a number of days from 0d to {MAX_DAYS_AGO}d"))?;
        today
            .checked_sub_days(Days::new(days))
            .ok_or_else(|| format!("{input} is too long ago"))?
    } else {
        NaiveDate::parse_from_str(input, "%Y-%m-%d").map_err(|_| {
            format!(
                "{input} isn't a date. Give one as YYYY-MM-DD, such as 2024-11-19, or as a \
                 number of days ago, such as 348d"
            )
        })?
    };

    if date > today {
        return Err(format!("{date} is in the future"));
    }
    Ok(date)
}

/// Whole days from a date to today.
pub fn days_since(date: NaiveDate, today: NaiveDate) -> i64 {
    (today - date).num_days()
}

/// A number of days in words, such as "1 day" or "348 days".
pub fn days(count: i64) -> String {
    plural(count, "day")
}

/// A grace period given as minutes, hours, days or weeks, such as `90m`,
/// `12h`, `3d` or `2w`, in seconds, or None for `never`.
pub fn parse_grace(input: &str) -> Result<Option<i64>, String> {
    if input.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let problem = || {
        format!(
            "{input} isn't a grace period. Give one in minutes, hours, days or weeks, such as \
             90m, 12h, 3d or 2w, or never"
        )
    };

    let unit = input.chars().last().ok_or_else(problem)?;
    let length = match unit.to_ascii_lowercase() {
        'm' => MINUTE,
        'h' => HOUR,
        'd' => DAY,
        'w' => WEEK,
        _ => return Err(problem()),
    };
    let number: i64 = input[..input.len() - 1].parse().map_err(|_| problem())?;
    let seconds = number.checked_mul(length).ok_or_else(problem)?;
    if !(MINUTE..=MAX_GRACE).contains(&seconds) {
        return Err(format!(
            "A grace period must be from 1m to {}.",
            days(MAX_GRACE / DAY)
        ));
    }
    Ok(Some(seconds))
}

/// A grace period in words, in the largest unit it is a whole number of,
/// such as "3 days", "12 hours" or "90 minutes".
pub fn length(seconds: i64) -> String {
    if seconds % DAY == 0 {
        plural(seconds / DAY, "day")
    } else if seconds % HOUR == 0 {
        plural(seconds / HOUR, "hour")
    } else {
        plural(seconds / MINUTE, "minute")
    }
}

/// A grace period as admins type it, in the largest of days, hours and
/// minutes it is a whole number of, such as `3d`, `12h` or `90m`.
pub fn short(seconds: i64) -> String {
    if seconds % DAY == 0 {
        format!("{}d", seconds / DAY)
    } else if seconds % HOUR == 0 {
        format!("{}h", seconds / HOUR)
    } else {
        format!("{}m", seconds / MINUTE)
    }
}

/// Roughly how long a number of seconds is, rounded down to days, hours or
/// minutes, such as "2 days" or "45 minutes".
pub fn about(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= DAY {
        plural(seconds / DAY, "day")
    } else if seconds >= HOUR {
        plural(seconds / HOUR, "hour")
    } else {
        plural(seconds / MINUTE, "minute")
    }
}

fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn today_is_in_utc() {
        // 2026-11-02 23:30 UTC, which is still November 2nd
        assert_eq!(today(1_793_662_200), date("2026-11-02"));
        assert_eq!(today(1_793_662_200 + 3600), date("2026-11-03"));
    }

    #[test]
    fn parses_dates_and_days_ago() {
        let today = date("2026-11-02");
        assert_eq!(parse("2024-11-19", today), Ok(date("2024-11-19")));
        assert_eq!(parse("348d", today), Ok(date("2025-11-19")));
        assert_eq!(parse("0d", today), Ok(today));
    }

    #[test]
    fn rejects_bad_dates() {
        let today = date("2026-11-02");
        for input in [
            "2024-13-01",
            "yesterday",
            "d",
            "-5d",
            "99999d",
            "2026-11-03",
        ] {
            assert!(parse(input, today).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn parses_grace_periods() {
        assert_eq!(parse_grace("90m"), Ok(Some(90 * MINUTE)));
        assert_eq!(parse_grace("12H"), Ok(Some(12 * HOUR)));
        assert_eq!(parse_grace("3d"), Ok(Some(3 * DAY)));
        assert_eq!(parse_grace("2w"), Ok(Some(2 * WEEK)));
        assert_eq!(parse_grace("Never"), Ok(None));
        for input in ["", "3", "d", "0m", "3y", "-1d", "400d", "9999999999999w"] {
            assert!(parse_grace(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn writes_grace_periods_as_admins_type_them() {
        assert_eq!(short(3 * DAY), "3d");
        assert_eq!(short(2 * WEEK), "14d");
        assert_eq!(short(HOUR), "1h");
        assert_eq!(short(90 * MINUTE), "90m");
        for seconds in [MINUTE, 90 * MINUTE, 12 * HOUR, 3 * DAY, 2 * WEEK] {
            assert_eq!(parse_grace(&short(seconds)), Ok(Some(seconds)));
        }
    }

    #[test]
    fn says_lengths_of_time() {
        assert_eq!(length(3 * DAY), "3 days");
        assert_eq!(length(2 * WEEK), "14 days");
        assert_eq!(length(HOUR), "1 hour");
        assert_eq!(length(90 * MINUTE), "90 minutes");
        assert_eq!(about(3 * DAY + 5 * HOUR), "3 days");
        assert_eq!(about(5 * HOUR + 59 * MINUTE), "5 hours");
        assert_eq!(about(59), "0 minutes");
        assert_eq!(about(-10), "0 minutes");
    }

    #[test]
    fn counts_days() {
        let today = date("2026-11-02");
        assert_eq!(days_since(date("2025-11-19"), today), 348);
        assert_eq!(days(1), "1 day");
        assert_eq!(days(0), "0 days");
        assert_eq!(days(348), "348 days");
    }
}
