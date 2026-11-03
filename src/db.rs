//! The SQLite database where lurker keeps what it knows about the users in
//! its rooms, so it survives restarts and reconnects.
//!
//! `users` has one row per person lurker has seen in a room. `user_keys`
//! records every nick and `ident@host` combination each user has been seen
//! with, which is how a user is recognized when they come back.
//! `memberships` records when each user was first and last seen in each
//! room.
//!
//! Times are whole seconds since 1970 UTC. In `sqlite3`,
//! `datetime(first_joined_at, 'unixepoch')` shows one as a date and time.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::casemap;
use crate::config::Room;

/// The schema, as the steps that build it. SQLite's `user_version` stores how
/// many steps a database has had, so an older database is brought up to date
/// by running the steps it is missing.
const MIGRATIONS: [&str; 1] = [SCHEMA_V1];

const SCHEMA_V1: &str = "
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    -- The nick the user was last seen with.
    nick TEXT NOT NULL,
    -- Their services account, while the server says they are logged in.
    account TEXT,
    created_at INTEGER NOT NULL
);

-- Every nick and ident@host combination a user has been seen with.
CREATE TABLE user_keys (
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Folded with lurker's case mapping, so lookups ignore case.
    nick TEXT NOT NULL,
    ident TEXT NOT NULL,
    -- Lowercased, since host names ignore case.
    host TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (nick, ident, host)
);
CREATE INDEX user_keys_user ON user_keys (user_id);

-- The rooms each user has been in.
CREATE TABLE memberships (
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    room TEXT NOT NULL CHECK (room IN ('purgatory', 'watchlist')),
    -- When lurker first saw them in the room. Time in the room counts from
    -- here, even when they have left and come back since.
    first_joined_at INTEGER NOT NULL,
    -- When lurker last saw them in the room. While they are in it, this is
    -- updated when they join, leave, or lurker loses sight of the room.
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, room)
);
";

/// How lurker sees a user's connection: their nick and `ident@host`.
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub nick: String,
    pub ident: String,
    pub host: String,
}

impl Key {
    pub fn new(nick: &str, ident: &str, host: &str) -> Key {
        Key {
            nick: nick.to_owned(),
            ident: ident.to_owned(),
            host: host.to_owned(),
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}!{}@{}", self.nick, self.ident, self.host)
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens the database, creating it when the file is new and bringing an
    /// older one up to date.
    pub fn open(path: &Path) -> Result<Db, String> {
        let conn = Connection::open(path)
            .map_err(|err| format!("opening database {}: {err}", path.display()))?;
        Db::setup(conn).map_err(|err| format!("database {}: {err}", path.display()))
    }

    /// An empty database in memory, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Db {
        Db::setup(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn setup(conn: Connection) -> Result<Db, String> {
        let fail = |err: rusqlite::Error| err.to_string();
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(fail)?;

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(fail)?;
        let latest = MIGRATIONS.len() as i64;
        if !(0..=latest).contains(&version) {
            return Err(format!(
                "it uses schema version {version}, but this version of lurker only knows up \
                 to {latest}. Run a newer lurker, or restore a backup made by this one"
            ));
        }
        for (step, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let version = step + 1;
            conn.execute_batch(&format!(
                "BEGIN; {sql} PRAGMA user_version = {version}; COMMIT;"
            ))
            .map_err(fail)?;
        }

        Ok(Db { conn })
    }

    /// The user who has been seen with exactly this nick and `ident@host`.
    pub fn find_user(&self, key: &Key) -> rusqlite::Result<Option<i64>> {
        let (nick, ident, host) = stored(key);
        self.conn
            .query_row(
                "SELECT user_id FROM user_keys WHERE nick = ?1 AND ident = ?2 AND host = ?3",
                params![nick, ident, host],
                |row| row.get(0),
            )
            .optional()
    }

    /// Adds a user first seen with this key.
    pub fn create_user(&self, key: &Key, now: i64) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO users (nick, created_at) VALUES (?1, ?2)",
            params![key.nick, now],
        )?;
        let id = self.conn.last_insert_rowid();
        self.saw_key(id, key, now)?;
        Ok(id)
    }

    /// Records that a user was seen with this key now, and that this is the
    /// nick they go by.
    pub fn saw_key(&self, user_id: i64, key: &Key, now: i64) -> rusqlite::Result<()> {
        let (nick, ident, host) = stored(key);
        self.conn.execute(
            "INSERT INTO user_keys (user_id, nick, ident, host, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT (nick, ident, host) DO UPDATE SET last_seen_at = excluded.last_seen_at",
            params![user_id, nick, ident, host, now],
        )?;
        self.conn.execute(
            "UPDATE users SET nick = ?2 WHERE id = ?1",
            params![user_id, key.nick],
        )?;
        Ok(())
    }

    /// Sets a user's services account, or clears it when they logged out.
    pub fn set_account(&self, user_id: i64, account: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE users SET account = ?2 WHERE id = ?1",
            params![user_id, account],
        )?;
        Ok(())
    }

    /// Records that a user is in a room now. Returns true the first time
    /// they are ever seen in it.
    pub fn arrived(&self, user_id: i64, room: Room, now: i64) -> rusqlite::Result<bool> {
        let added = self.conn.execute(
            "INSERT INTO memberships (user_id, room, first_joined_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT (user_id, room) DO NOTHING",
            params![user_id, room.as_str(), now],
        )?;
        if added == 0 {
            self.saw_in_room(user_id, room, now)?;
        }
        Ok(added == 1)
    }

    /// Records that a user was in a room now, such as when they leave it.
    pub fn saw_in_room(&self, user_id: i64, room: Room, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE memberships SET last_seen_at = ?3 WHERE user_id = ?1 AND room = ?2",
            params![user_id, room.as_str(), now],
        )?;
        Ok(())
    }

    /// When a user was first seen in a room, if they ever were.
    #[cfg(test)]
    pub fn first_joined(&self, user_id: i64, room: Room) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT first_joined_at FROM memberships WHERE user_id = ?1 AND room = ?2",
                params![user_id, room.as_str()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    /// A user's nick, account, and the number of keys they have been seen
    /// with, for tests.
    #[cfg(test)]
    pub fn user(&self, user_id: i64) -> (String, Option<String>, i64) {
        self.conn
            .query_row(
                "SELECT nick, account, (SELECT count(*) FROM user_keys WHERE user_id = id)
                 FROM users WHERE id = ?1",
                params![user_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    /// How many users there are, for tests.
    #[cfg(test)]
    pub fn user_count(&self) -> i64 {
        self.conn
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap()
    }
}

/// A key as `user_keys` stores it: the nick folded and the host lowercased.
fn stored(key: &Key) -> (String, &str, String) {
    (
        casemap::fold(&key.nick),
        &key.ident,
        key.host.to_ascii_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_schema() {
        let db = Db::open_in_memory();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn refuses_a_newer_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        let err = Db::setup(conn).err().unwrap();
        assert!(err.contains("version 99"), "{err}");
    }

    #[test]
    fn finds_users_by_key_ignoring_case() {
        let db = Db::open_in_memory();
        let id = db
            .create_user(&Key::new("Alice", "~a", "Fish-1.chat"), 100)
            .unwrap();

        assert_eq!(
            db.find_user(&Key::new("alice", "~a", "fish-1.CHAT"))
                .unwrap(),
            Some(id)
        );
        assert_eq!(
            db.find_user(&Key::new("alice", "~A", "fish-1.chat"))
                .unwrap(),
            None
        );
        assert_eq!(
            db.find_user(&Key::new("alice", "~a", "fish-2.chat"))
                .unwrap(),
            None
        );
        assert_eq!(
            db.find_user(&Key::new("alice_", "~a", "fish-1.chat"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn records_new_keys_and_nicks() {
        let db = Db::open_in_memory();
        let id = db.create_user(&Key::new("alice", "~a", "h"), 100).unwrap();
        db.saw_key(id, &Key::new("alice_", "~a", "h"), 200).unwrap();
        db.saw_key(id, &Key::new("alice_", "~a", "h"), 300).unwrap();

        assert_eq!(db.user(id), ("alice_".to_owned(), None, 2));
        assert_eq!(
            db.find_user(&Key::new("ALICE_", "~a", "h")).unwrap(),
            Some(id)
        );
    }

    #[test]
    fn keeps_the_first_join() {
        let db = Db::open_in_memory();
        let id = db.create_user(&Key::new("alice", "~a", "h"), 100).unwrap();

        assert!(db.arrived(id, Room::Purgatory, 100).unwrap());
        db.saw_in_room(id, Room::Purgatory, 150).unwrap();
        assert!(!db.arrived(id, Room::Purgatory, 200).unwrap());
        assert!(db.arrived(id, Room::Watchlist, 300).unwrap());

        assert_eq!(db.first_joined(id, Room::Purgatory), Some(100));
        assert_eq!(db.first_joined(id, Room::Watchlist), Some(300));
    }

    #[test]
    fn sets_and_clears_accounts() {
        let db = Db::open_in_memory();
        let id = db.create_user(&Key::new("alice", "~a", "h"), 100).unwrap();
        db.set_account(id, Some("alice")).unwrap();
        assert_eq!(db.user(id).1.as_deref(), Some("alice"));
        db.set_account(id, None).unwrap();
        assert_eq!(db.user(id).1, None);
    }
}
