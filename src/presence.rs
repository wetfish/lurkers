//! Who is in lurker's rooms right now, and the users in the database behind
//! them.
//!
//! When lurker joins a room it asks the server who is there with WHO, and
//! after that it follows joins, parts, kicks, quits, nick changes, host
//! changes and account changes. Each person present is linked to a user in
//! the database: the one in the other room under the same nick, since that
//! is the same connection, otherwise the user last seen with exactly the
//! same nick and `ident@host`, otherwise a new user.

use std::collections::HashMap;

use irc::client::prelude::Command;

use crate::casemap;
use crate::config::Room;
use crate::db::{Db, Key};

/// The token lurker tags its WHOX queries with, to recognize the replies.
pub const WHOX_TOKEN: &str = "152";

/// What the server says about someone's services account.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Account<'a> {
    /// The server didn't say, because it lacks the capability that would.
    Unknown,
    LoggedOut,
    LoggedIn(&'a str),
}

impl<'a> Account<'a> {
    /// The account as extended JOIN, ACCOUNT and WHOX replies give it, which
    /// use `*` or `0` for someone who isn't logged in.
    pub fn parse(value: &'a str) -> Account<'a> {
        match value {
            "" | "*" | "0" => Account::LoggedOut,
            name => Account::LoggedIn(name),
        }
    }
}

/// What a person showing up in a room meant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arrival {
    /// They were already known to be there.
    AlreadyHere,
    /// They are back in a room they have been in before.
    Returned,
    /// The first time they are ever seen in the room.
    First,
}

#[derive(Debug, Clone)]
struct Member {
    key: Key,
    user_id: i64,
}

#[derive(Default)]
pub struct Presence {
    /// Whether the server supports WHOX, which also reports accounts.
    whox: bool,
    /// The people in each room, by folded nick.
    rooms: HashMap<Room, HashMap<String, Member>>,
}

impl Presence {
    /// Notes the server's features from its ISUPPORT reply.
    pub fn isupport(&mut self, tokens: &[String]) {
        if tokens.iter().any(|t| t == "WHOX" || t.starts_with("WHOX=")) {
            self.whox = true;
        }
    }

    /// The query that lists who is in a channel, sent when lurker joins it.
    pub fn who(&self, channel: &str) -> Command {
        if self.whox {
            Command::Raw(
                "WHO".to_owned(),
                vec![channel.to_owned(), format!("%tcuhnfa,{WHOX_TOKEN}")],
            )
        } else {
            Command::WHO(Some(channel.to_owned()), None)
        }
    }

    /// Someone is in a room: they joined it, or a WHO reply listed them.
    pub fn arrived(
        &mut self,
        db: &Db,
        room: Room,
        key: Key,
        account: Account,
        now: i64,
    ) -> rusqlite::Result<Arrival> {
        let folded = casemap::fold(&key.nick);

        if let Some(member) = self.room_mut(room).get_mut(&folded) {
            let user_id = member.user_id;
            if member.key != key {
                db.saw_key(user_id, &key, now)?;
                member.key = key;
            }
            set_account(db, user_id, account)?;
            return Ok(Arrival::AlreadyHere);
        }

        let user_id = match self.find(&folded) {
            Some(member) => member.user_id,
            None => match db.find_user(&key)? {
                Some(user_id) => user_id,
                None => db.create_user(&key, now)?,
            },
        };
        db.saw_key(user_id, &key, now)?;
        set_account(db, user_id, account)?;
        let first = db.arrived(user_id, room, now)?;

        self.room_mut(room).insert(folded, Member { key, user_id });
        Ok(if first {
            Arrival::First
        } else {
            Arrival::Returned
        })
    }

    /// Someone left a room or was kicked from it. Returns who they were, if
    /// lurker knew they were there.
    pub fn departed(
        &mut self,
        db: &Db,
        room: Room,
        nick: &str,
        now: i64,
    ) -> rusqlite::Result<Option<Key>> {
        match self.room_mut(room).remove(&casemap::fold(nick)) {
            Some(member) => {
                db.saw_in_room(member.user_id, room, now)?;
                Ok(Some(member.key))
            }
            None => Ok(None),
        }
    }

    /// Someone disconnected, which takes them out of both rooms.
    pub fn quit(&mut self, db: &Db, nick: &str, now: i64) -> rusqlite::Result<Option<Key>> {
        let mut gone = None;
        for room in Room::ALL {
            if let Some(key) = self.departed(db, room, nick, now)? {
                gone = Some(key);
            }
        }
        Ok(gone)
    }

    /// Someone changed nick. Returns their new key, if lurker knew them.
    pub fn renamed(
        &mut self,
        db: &Db,
        old: &str,
        new: &str,
        now: i64,
    ) -> rusqlite::Result<Option<Key>> {
        let changed = self.update(old, |key| key.nick = new.to_owned());
        if let Some((user_id, key)) = &changed {
            db.saw_key(*user_id, key, now)?;
        }
        Ok(changed.map(|(_, key)| key))
    }

    /// Someone's ident or host changed while they stayed connected.
    pub fn host_changed(
        &mut self,
        db: &Db,
        nick: &str,
        ident: &str,
        host: &str,
        now: i64,
    ) -> rusqlite::Result<Option<Key>> {
        let changed = self.update(nick, |key| {
            key.ident = ident.to_owned();
            key.host = host.to_owned();
        });
        if let Some((user_id, key)) = &changed {
            db.saw_key(*user_id, key, now)?;
        }
        Ok(changed.map(|(_, key)| key))
    }

    /// Someone logged in to or out of their services account.
    pub fn account_changed(&self, db: &Db, nick: &str, account: Account) -> rusqlite::Result<()> {
        match self.find(&casemap::fold(nick)) {
            Some(member) => set_account(db, member.user_id, account),
            None => Ok(()),
        }
    }

    /// lurker left a room or lost its connection, so it no longer sees who
    /// is there. Returns how many people it stopped following.
    pub fn lost_room(&mut self, db: &Db, room: Room, now: i64) -> rusqlite::Result<usize> {
        let members = std::mem::take(self.room_mut(room));
        for member in members.values() {
            db.saw_in_room(member.user_id, room, now)?;
        }
        Ok(members.len())
    }

    /// How many people lurker knows are in a room.
    pub fn count(&self, room: Room) -> usize {
        self.rooms.get(&room).map_or(0, HashMap::len)
    }

    /// The user behind a nick in a room, for tests.
    #[cfg(test)]
    pub fn user_in(&self, room: Room, nick: &str) -> Option<i64> {
        self.rooms
            .get(&room)?
            .get(&casemap::fold(nick))
            .map(|member| member.user_id)
    }

    fn room_mut(&mut self, room: Room) -> &mut HashMap<String, Member> {
        self.rooms.entry(room).or_default()
    }

    /// Someone in either room under this folded nick.
    fn find(&self, folded: &str) -> Option<&Member> {
        self.rooms.values().find_map(|members| members.get(folded))
    }

    /// Changes someone's key in every room they are in. Returns their user
    /// and new key, if they are in either room.
    fn update(&mut self, nick: &str, change: impl Fn(&mut Key)) -> Option<(i64, Key)> {
        let folded = casemap::fold(nick);
        let mut changed = None;

        for members in self.rooms.values_mut() {
            if let Some(mut member) = members.remove(&folded) {
                change(&mut member.key);
                changed = Some((member.user_id, member.key.clone()));
                members.insert(casemap::fold(&member.key.nick), member);
            }
        }
        changed
    }
}

fn set_account(db: &Db, user_id: i64, account: Account) -> rusqlite::Result<()> {
    match account {
        Account::Unknown => Ok(()),
        Account::LoggedOut => db.set_account(user_id, None),
        Account::LoggedIn(name) => db.set_account(user_id, Some(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(nick: &str, host: &str) -> Key {
        Key::new(nick, "~u", host)
    }

    #[test]
    fn creates_a_user_the_first_time() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();

        let arrival = presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        assert_eq!(arrival, Arrival::First);
        assert_eq!(presence.count(Room::Purgatory), 1);
        assert_eq!(db.user_count(), 1);
    }

    #[test]
    fn recognizes_the_same_key_after_leaving() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        let first = presence.user_in(Room::Purgatory, "alice").unwrap();

        presence.quit(&db, "Alice", 200).unwrap();
        assert_eq!(presence.count(Room::Purgatory), 0);

        let arrival = presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "H"),
                Account::Unknown,
                300,
            )
            .unwrap();
        assert_eq!(arrival, Arrival::Returned);
        assert_eq!(presence.user_in(Room::Purgatory, "alice"), Some(first));
        assert_eq!(db.first_joined(first, Room::Purgatory), Some(100));
    }

    #[test]
    fn a_new_host_is_a_new_user_for_now() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "old"),
                Account::Unknown,
                100,
            )
            .unwrap();
        presence.quit(&db, "alice", 200).unwrap();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "new"),
                Account::Unknown,
                300,
            )
            .unwrap();
        assert_eq!(db.user_count(), 2);
    }

    #[test]
    fn listing_someone_twice_changes_nothing() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        let arrival = presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                101,
            )
            .unwrap();
        assert_eq!(arrival, Arrival::AlreadyHere);
        assert_eq!(db.user_count(), 1);
    }

    #[test]
    fn the_same_nick_in_both_rooms_is_one_user() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        let arrival = presence
            .arrived(
                &db,
                Room::Watchlist,
                key("alice", "h"),
                Account::Unknown,
                200,
            )
            .unwrap();
        assert_eq!(arrival, Arrival::First);
        assert_eq!(db.user_count(), 1);
    }

    #[test]
    fn follows_nick_changes() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        presence
            .arrived(
                &db,
                Room::Watchlist,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        let user = presence.user_in(Room::Purgatory, "alice").unwrap();

        let new = presence.renamed(&db, "alice", "alice_", 200).unwrap();
        assert_eq!(new, Some(key("alice_", "h")));
        assert_eq!(presence.user_in(Room::Purgatory, "alice_"), Some(user));
        assert_eq!(presence.user_in(Room::Watchlist, "alice_"), Some(user));
        assert_eq!(presence.user_in(Room::Purgatory, "alice"), None);

        // Coming back later under the new nick finds the same user
        presence.quit(&db, "alice_", 300).unwrap();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice_", "h"),
                Account::Unknown,
                400,
            )
            .unwrap();
        assert_eq!(presence.user_in(Room::Purgatory, "alice_"), Some(user));
        assert_eq!(db.user(user).0, "alice_");
    }

    #[test]
    fn follows_host_changes() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "old"),
                Account::Unknown,
                100,
            )
            .unwrap();
        let user = presence.user_in(Room::Purgatory, "alice").unwrap();

        presence
            .host_changed(&db, "alice", "~u", "new", 200)
            .unwrap();
        assert_eq!(db.find_user(&key("alice", "new")).unwrap(), Some(user));
    }

    #[test]
    fn records_accounts() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::parse("alice"),
                100,
            )
            .unwrap();
        let user = presence.user_in(Room::Purgatory, "alice").unwrap();
        assert_eq!(db.user(user).1.as_deref(), Some("alice"));

        presence
            .account_changed(&db, "alice", Account::parse("*"))
            .unwrap();
        assert_eq!(db.user(user).1, None);
    }

    #[test]
    fn losing_a_room_forgets_who_was_there() {
        let db = Db::open_in_memory();
        let mut presence = Presence::default();
        presence
            .arrived(
                &db,
                Room::Purgatory,
                key("alice", "h"),
                Account::Unknown,
                100,
            )
            .unwrap();
        presence
            .arrived(&db, Room::Purgatory, key("bob", "h"), Account::Unknown, 100)
            .unwrap();
        assert_eq!(presence.lost_room(&db, Room::Purgatory, 200).unwrap(), 2);
        assert_eq!(presence.count(Room::Purgatory), 0);
    }

    #[test]
    fn asks_with_whox_when_the_server_supports_it() {
        let mut presence = Presence::default();
        assert_eq!(
            presence.who("#purgatory"),
            Command::WHO(Some("#purgatory".to_owned()), None)
        );

        presence.isupport(&["SAFELIST".to_owned(), "WHOX".to_owned()]);
        assert_eq!(
            presence.who("#purgatory"),
            Command::Raw(
                "WHO".to_owned(),
                vec!["#purgatory".to_owned(), "%tcuhnfa,152".to_owned()]
            )
        );
    }
}
