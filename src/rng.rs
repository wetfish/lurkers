//! Random numbers for picking messages and ping times. Nothing here needs
//! to be unpredictable, so a small splitmix64 generator seeded from the
//! clock is enough, and tests can seed it to get the same picks every time.

use std::time::{SystemTime, UNIX_EPOCH};

pub struct Rng(u64);

impl Rng {
    /// A generator that always gives the same numbers, for tests.
    #[cfg(test)]
    pub fn seeded(seed: u64) -> Rng {
        Rng(seed)
    }

    /// A generator seeded from the clock and the process id.
    pub fn from_clock() -> Rng {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        Rng(nanos ^ (u64::from(std::process::id()) << 32))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number from 0 up to, but not including, `limit`. 0 when `limit`
    /// is 0.
    pub fn below(&mut self, limit: u64) -> u64 {
        if limit == 0 {
            0
        } else {
            self.next_u64() % limit
        }
    }

    /// One of the items, or None when there are none.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let index = self.below(items.len() as u64) as usize;
        items.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_below_the_limit() {
        let mut rng = Rng::seeded(7);
        assert!((0..1000).all(|_| rng.below(5) < 5));
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn picks_every_item_eventually() {
        let mut rng = Rng::seeded(7);
        let items = ["a", "b", "c"];
        let mut seen = [false; 3];
        for _ in 0..100 {
            let item = rng.pick(&items).unwrap();
            seen[items.iter().position(|i| i == item).unwrap()] = true;
        }
        assert_eq!(seen, [true; 3]);
        assert_eq!(rng.pick::<u8>(&[]), None);
    }

    #[test]
    fn the_same_seed_gives_the_same_numbers() {
        let (mut a, mut b) = (Rng::seeded(42), Rng::seeded(42));
        assert!((0..10).all(|_| a.next_u64() == b.next_u64()));
    }
}
