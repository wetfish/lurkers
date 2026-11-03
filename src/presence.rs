//! Who is in the channels lurker is in right now, by nick: its two rooms and
//! the channels admins asked it to join. lurker learns this from the
//! server's list of names when it joins a channel, then follows joins,
//! parts, kicks, quits and nick changes. Nothing here is stored.

use std::collections::{HashMap, HashSet};

use crate::casemap;

#[derive(Default)]
pub struct Presence {
    /// The folded nicks in each channel, by folded channel name.
    channels: HashMap<String, HashSet<String>>,
}

impl Presence {
    pub fn add(&mut self, channel: &str, nick: &str) {
        self.channels
            .entry(casemap::fold(channel))
            .or_default()
            .insert(casemap::fold(nick));
    }

    /// Someone left a channel or was kicked from it. Returns whether they
    /// were there.
    pub fn remove(&mut self, channel: &str, nick: &str) -> bool {
        self.channels
            .get_mut(&casemap::fold(channel))
            .is_some_and(|nicks| nicks.remove(&casemap::fold(nick)))
    }

    /// Someone disconnected, which takes them out of every channel. Returns
    /// whether they were in any.
    pub fn quit(&mut self, nick: &str) -> bool {
        let nick = casemap::fold(nick);
        let mut found = false;
        for nicks in self.channels.values_mut() {
            found |= nicks.remove(&nick);
        }
        found
    }

    /// Someone changed nick.
    pub fn rename(&mut self, old: &str, new: &str) {
        let (old, new) = (casemap::fold(old), casemap::fold(new));
        for nicks in self.channels.values_mut() {
            if nicks.remove(&old) {
                nicks.insert(new.clone());
            }
        }
    }

    /// lurker left a channel or lost its connection, so it no longer sees
    /// who is there.
    pub fn clear(&mut self, channel: &str) {
        self.channels.remove(&casemap::fold(channel));
    }

    pub fn contains(&self, channel: &str, nick: &str) -> bool {
        self.channels
            .get(&casemap::fold(channel))
            .is_some_and(|nicks| nicks.contains(&casemap::fold(nick)))
    }

    /// How many people lurker knows are in a channel.
    pub fn count(&self, channel: &str) -> usize {
        self.channels
            .get(&casemap::fold(channel))
            .map_or(0, HashSet::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_people_in_and_out() {
        let mut presence = Presence::default();
        presence.add("#purgatory", "Alice");
        presence.add("#wetfish", "alice");
        presence.add("#Purgatory", "bob");
        assert!(presence.contains("#PURGATORY", "ALICE"));
        assert_eq!(presence.count("#purgatory"), 2);

        assert!(presence.remove("#purgatory", "bob"));
        assert!(!presence.remove("#purgatory", "bob"));
        assert!(presence.quit("alice"));
        assert!(!presence.quit("alice"));
        assert_eq!(presence.count("#wetfish"), 0);
    }

    #[test]
    fn follows_nick_changes() {
        let mut presence = Presence::default();
        presence.add("#wetfish", "alice");
        presence.rename("alice", "alice_");
        assert!(presence.contains("#wetfish", "alice_"));
        assert!(!presence.contains("#wetfish", "alice"));
        assert!(!presence.contains("#purgatory", "alice_"));
    }

    #[test]
    fn forgets_a_channel_lurker_left() {
        let mut presence = Presence::default();
        presence.add("#purgatory", "alice");
        presence.clear("#Purgatory");
        assert_eq!(presence.count("#purgatory"), 0);
    }
}
