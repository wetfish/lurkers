//! When lurker pings each lurker: once a week at most, at a random time, and
//! never two pings in a room within a few minutes of each other.

use crate::rng::Rng;

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// The least time between two pings in the same room, so a room with many
/// lurkers is never flooded.
pub const ROOM_GAP: i64 = 20 * 60;

/// A lurker's first ping: 3 to 7 days after they first joined their room.
/// A lurker who joined longer ago than that, such as one marked before this
/// version of lurker, is pinged at a random time within the next week
/// instead, so they don't all come due at once.
pub fn first_ping(here_since: i64, now: i64, rng: &mut Rng) -> i64 {
    let at = here_since + 3 * DAY + random(4 * DAY, rng);
    if at > now {
        at
    } else {
        now + random(7 * DAY, rng)
    }
}

/// The ping after one sent now: a week later, plus up to 2 days, so it
/// never comes more than once a week and drifts around the clock.
pub fn next_ping(now: i64, rng: &mut Rng) -> i64 {
    now + 7 * DAY + random(2 * DAY, rng)
}

fn random(limit: i64, rng: &mut Rng) -> i64 {
    rng.below(limit as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_pings_come_3_to_7_days_after_joining() {
        let mut rng = Rng::seeded(1);
        for _ in 0..1000 {
            let at = first_ping(1_000_000, 1_000_000, &mut rng);
            assert!((1_000_000 + 3 * DAY..1_000_000 + 7 * DAY).contains(&at));
        }
    }

    #[test]
    fn long_time_lurkers_are_spread_over_the_next_week() {
        let mut rng = Rng::seeded(1);
        let now = 100 * DAY;
        for _ in 0..1000 {
            let at = first_ping(0, now, &mut rng);
            assert!((now..now + 7 * DAY).contains(&at));
        }
    }

    #[test]
    fn pings_come_at_most_once_a_week() {
        let mut rng = Rng::seeded(1);
        for _ in 0..1000 {
            let at = next_ping(0, &mut rng);
            assert!((7 * DAY..9 * DAY).contains(&at));
        }
    }
}
