//! The units lurker can say a length of time in, from seconds to dog years.

const MINUTE: f64 = 60.0;
const HOUR: f64 = 60.0 * MINUTE;
const DAY: f64 = 24.0 * HOUR;

pub struct Unit {
    /// The name templates use, as in `{idle:seconds}`
    pub name: &'static str,
    /// How many seconds one of it lasts
    seconds: f64,
    singular: &'static str,
    plural: &'static str,
}

pub static UNITS: [Unit; 20] = [
    unit("seconds", 1.0, "second", "seconds"),
    unit("minutes", MINUTE, "minute", "minutes"),
    unit("hours", HOUR, "hour", "hours"),
    unit("days", DAY, "day", "days"),
    // 70 beats a minute
    unit("heartbeats", MINUTE / 70.0, "heartbeat", "heartbeats"),
    // 1,200 beats a minute
    unit(
        "hummingbird_heartbeats",
        MINUTE / 1200.0,
        "hummingbird heartbeat",
        "hummingbird heartbeats",
    ),
    unit(
        "bitcoin_blocks",
        10.0 * MINUTE,
        "Bitcoin block",
        "Bitcoin blocks",
    ),
    // A millionth of a century, a classic lecture length
    unit(
        "microcenturies",
        36_525.0 * DAY / 1_000_000.0,
        "microcentury",
        "microcenturies",
    ),
    unit(
        "iss_orbits",
        92.9 * MINUTE,
        "orbit of the ISS",
        "orbits of the ISS",
    ),
    unit(
        "shifts",
        8.0 * HOUR,
        "eight-hour shift",
        "eight-hour shifts",
    ),
    unit(
        "mayfly_lifetimes",
        DAY,
        "mayfly lifetime",
        "mayfly lifetimes",
    ),
    unit(
        "sols",
        DAY + 39.0 * MINUTE + 35.0,
        "sol on Mars",
        "sols on Mars",
    ),
    unit(
        "iodine_half_lives",
        8.02 * DAY,
        "half-life of iodine-131",
        "half-lives of iodine-131",
    ),
    unit(
        "fruit_fly_generations",
        10.0 * DAY,
        "generation of fruit flies",
        "generations of fruit flies",
    ),
    unit(
        "mosquito_generations",
        14.0 * DAY,
        "generation of mosquitoes",
        "generations of mosquitoes",
    ),
    unit("fortnights", 14.0 * DAY, "fortnight", "fortnights"),
    unit(
        "menstrual_cycles",
        28.0 * DAY,
        "menstrual cycle",
        "menstrual cycles",
    ),
    unit("full_moons", 29.53 * DAY, "full moon", "full moons"),
    unit(
        "mercury_years",
        88.0 * DAY,
        "year on Mercury",
        "years on Mercury",
    ),
    unit("dog_years", 365.25 / 7.0 * DAY, "dog year", "dog years"),
];

const fn unit(
    name: &'static str,
    seconds: f64,
    singular: &'static str,
    plural: &'static str,
) -> Unit {
    Unit {
        name,
        seconds,
        singular,
        plural,
    }
}

/// The unit with this name, as templates give it.
pub fn find(name: &str) -> Option<&'static Unit> {
    UNITS.iter().find(|unit| unit.name == name)
}

/// Days, the unit used when a template names none.
pub fn days() -> &'static Unit {
    &UNITS[3]
}

impl Unit {
    /// How many whole units fit in a number of seconds.
    pub fn count(&self, seconds: i64) -> i64 {
        (seconds.max(0) as f64 / self.seconds).floor() as i64
    }

    /// A number of seconds in this unit, such as "30,067,200 seconds" or
    /// "1 dog year".
    pub fn format(&self, seconds: i64) -> String {
        let count = self.count(seconds);
        let name = if count == 1 {
            self.singular
        } else {
            self.plural
        };
        format!("{} {name}", thousands(count))
    }
}

/// A number with commas between groups of three digits.
pub fn thousands(number: i64) -> String {
    let digits = number.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if number < 0 {
        grouped.insert(0, '-');
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAYS_348: i64 = 348 * 86_400;

    #[test]
    fn groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(30_067_200), "30,067,200");
        assert_eq!(thousands(-1_234), "-1,234");
    }

    #[test]
    fn says_lengths_of_time_in_each_unit() {
        let say = |name, seconds| find(name).unwrap().format(seconds);
        assert_eq!(say("seconds", DAYS_348), "30,067,200 seconds");
        assert_eq!(say("days", DAYS_348), "348 days");
        assert_eq!(say("days", 86_400), "1 day");
        assert_eq!(say("heartbeats", DAYS_348), "35,078,400 heartbeats");
        assert_eq!(
            say("mosquito_generations", DAYS_348),
            "24 generations of mosquitoes"
        );
        assert_eq!(say("menstrual_cycles", DAYS_348), "12 menstrual cycles");
        assert_eq!(say("full_moons", DAYS_348), "11 full moons");
        assert_eq!(say("mercury_years", DAYS_348), "3 years on Mercury");
        assert_eq!(say("dog_years", DAYS_348), "6 dog years");
        assert_eq!(say("dog_years", 60 * 86_400), "1 dog year");
    }

    #[test]
    fn every_unit_has_a_distinct_name() {
        for (i, unit) in UNITS.iter().enumerate() {
            assert!(UNITS[i + 1..].iter().all(|other| other.name != unit.name));
            assert!(unit.seconds > 0.0);
        }
        assert_eq!(days().name, "days");
    }
}
