//! Who is in lurker's rooms right now, by nick. lurker learns this from the
//! server's list of names when it joins a room, then follows joins, parts,
//! kicks, quits and nick changes. Nothing here is stored: it only tells
//! which marked lurkers are around.

use std::collections::{HashMap, HashSet};

use crate::casemap;
use crate::config::Room;

#[derive(Default)]
pub struct Presence {
    /// The folded nicks in each room.
    rooms: HashMap<Room, HashSet<String>>,
}

impl Presence {
    pub fn add(&mut self, room: Room, nick: &str) {
        self.rooms
            .entry(room)
            .or_default()
            .insert(casemap::fold(nick));
    }

    /// Someone left a room or was kicked from it. Returns whether they were
    /// there.
    pub fn remove(&mut self, room: Room, nick: &str) -> bool {
        self.rooms
            .get_mut(&room)
            .is_some_and(|nicks| nicks.remove(&casemap::fold(nick)))
    }

    /// Someone disconnected, which takes them out of both rooms. Returns
    /// the rooms they were in.
    pub fn quit(&mut self, nick: &str) -> Vec<Room> {
        Room::ALL
            .into_iter()
            .filter(|&room| self.remove(room, nick))
            .collect()
    }

    /// Someone changed nick.
    pub fn rename(&mut self, old: &str, new: &str) {
        for room in self.quit(old) {
            self.add(room, new);
        }
    }

    /// lurker left a room or lost its connection, so it no longer sees who
    /// is there.
    pub fn clear(&mut self, room: Room) {
        self.rooms.remove(&room);
    }

    pub fn contains(&self, room: Room, nick: &str) -> bool {
        self.rooms
            .get(&room)
            .is_some_and(|nicks| nicks.contains(&casemap::fold(nick)))
    }

    /// How many people lurker knows are in a room.
    pub fn count(&self, room: Room) -> usize {
        self.rooms.get(&room).map_or(0, HashSet::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_people_in_and_out() {
        let mut presence = Presence::default();
        presence.add(Room::Purgatory, "Alice");
        presence.add(Room::Watchlist, "alice");
        presence.add(Room::Purgatory, "bob");
        assert!(presence.contains(Room::Purgatory, "ALICE"));
        assert_eq!(presence.count(Room::Purgatory), 2);

        assert!(presence.remove(Room::Purgatory, "bob"));
        assert!(!presence.remove(Room::Purgatory, "bob"));
        assert_eq!(presence.quit("alice"), [Room::Purgatory, Room::Watchlist]);
        assert_eq!(presence.count(Room::Watchlist), 0);
    }

    #[test]
    fn follows_nick_changes() {
        let mut presence = Presence::default();
        presence.add(Room::Watchlist, "alice");
        presence.rename("alice", "alice_");
        assert!(presence.contains(Room::Watchlist, "alice_"));
        assert!(!presence.contains(Room::Watchlist, "alice"));
        assert!(!presence.contains(Room::Purgatory, "alice_"));
    }

    #[test]
    fn forgets_a_room_lurker_left() {
        let mut presence = Presence::default();
        presence.add(Room::Purgatory, "alice");
        presence.clear(Room::Purgatory);
        assert_eq!(presence.count(Room::Purgatory), 0);
    }
}
