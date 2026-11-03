//! Dates as admins give them and lurker reports them. Days are counted in
//! UTC.

use chrono::{DateTime, Days, NaiveDate};

/// The longest ago a number of days such as `348d` may reach, about 100
/// years, to catch typos.
const MAX_DAYS_AGO: u64 = 36_500;

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
    if count == 1 {
        "1 day".to_owned()
    } else {
        format!("{count} days")
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
    fn counts_days() {
        let today = date("2026-11-02");
        assert_eq!(days_since(date("2025-11-19"), today), 348);
        assert_eq!(days(1), "1 day");
        assert_eq!(days(0), "0 days");
        assert_eq!(days(348), "348 days");
    }
}
