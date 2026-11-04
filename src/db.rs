//! The SQLite database where lurker keeps the lurkers admins have marked, so
//! they survive restarts.
//!
//! `lurkers` has one row per marked user. `used_pings` remembers which ping
//! messages each lurker has heard, and `channels` the channels admins asked
//! lurker to join besides its rooms. `nudges` is the nudge list: the people
//! admins asked lurker to remind to chat, kept after each nudge ends. Nobody else is recorded: people who
//! aren't marked are left alone.
//!
//! Times are whole seconds since 1970 UTC. In `sqlite3`,
//! `datetime(marked_at, 'unixepoch')` shows one as a date and time. Dates
//! are `YYYY-MM-DD`.

use std::path::Path;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};

use crate::casemap;
use crate::config::Room;

/// The schema, as the steps that build it. SQLite's `user_version` stores how
/// many steps a database has had, so an older database is brought up to date
/// by running the steps it is missing.
const MIGRATIONS: [&str; 8] = [
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
];

const SCHEMA_V1: &str = "
CREATE TABLE lurkers (
    id INTEGER PRIMARY KEY,
    -- The nick the admin gave, updated when lurker sees them change nick.
    nick TEXT NOT NULL,
    -- The nick folded with IRC's case mapping, to look them up.
    nick_key TEXT NOT NULL UNIQUE,
    -- 'purgatory' for trusted users and 'watchlist' for untrusted ones.
    room TEXT NOT NULL CHECK (room IN ('purgatory', 'watchlist')),
    -- The year they were most active, and how often they chatted then.
    peak_year INTEGER,
    peak_frequency TEXT CHECK (peak_frequency IN ('daily', 'weekly', 'monthly')),
    -- When lurker first saw them in their room since they were marked for
    -- it. NULL until then.
    here_since INTEGER,
    -- The admin who last marked them, and when.
    marked_by TEXT NOT NULL,
    marked_at INTEGER NOT NULL
);

-- The channels a lurker idled in.
CREATE TABLE lurker_channels (
    lurker_id INTEGER NOT NULL REFERENCES lurkers (id) ON DELETE CASCADE,
    -- The channel name as the admin gave it.
    channel TEXT NOT NULL,
    -- The channel name folded with IRC's case mapping.
    channel_key TEXT NOT NULL,
    -- When they went silent there: when they last spoke, or were first seen
    -- if they never spoke.
    since TEXT NOT NULL,
    PRIMARY KEY (lurker_id, channel_key)
);
";

const SCHEMA_V2: &str = "
-- When lurker next pings them, once their time in their room has started.
ALTER TABLE lurkers ADD COLUMN next_ping_at INTEGER;
-- When lurker last made a fuss about them speaking.
ALTER TABLE lurkers ADD COLUMN fanfare_at INTEGER;

-- The ping messages each lurker has heard since they last heard them all,
-- by their number in messages.toml.
CREATE TABLE used_pings (
    lurker_id INTEGER NOT NULL REFERENCES lurkers (id) ON DELETE CASCADE,
    template INTEGER NOT NULL,
    PRIMARY KEY (lurker_id, template)
);
";

const SCHEMA_V3: &str = "
-- How many seconds after their warning a lurker is kicked from lurker's
-- channels. NULL means never.
ALTER TABLE lurkers ADD COLUMN grace INTEGER;
-- When lurker warned them that they would be kicked.
ALTER TABLE lurkers ADD COLUMN warned_at INTEGER;

-- The channels admins asked lurker to join besides its rooms, where it warns
-- and kicks lurkers when it is allowed to.
CREATE TABLE channels (
    -- The channel name folded with IRC's case mapping.
    channel_key TEXT PRIMARY KEY,
    -- The channel name as the admin gave it.
    channel TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL
);
";

const SCHEMA_V4: &str = "
-- When lurker last saw them say something, in any channel it is in. A
-- lurker who speaks after their warning is no longer kicked.
ALTER TABLE lurkers ADD COLUMN spoke_at INTEGER;
";

const SCHEMA_V5: &str = "
-- When they went silent, as YYYY-MM-DD: when they last spoke anywhere, or
-- were first seen if they never spoke. Lurkers marked before this was added
-- keep the most recent of their channels' dates.
ALTER TABLE lurkers ADD COLUMN silent_since TEXT;
UPDATE lurkers SET silent_since =
    (SELECT max(since) FROM lurker_channels WHERE lurker_id = lurkers.id);
DROP TABLE lurker_channels;
";

const SCHEMA_V6: &str = "
-- When lurker last invited them to their room. Empty for everyone at first,
-- so lurkers sajoined before lurker sent invites are invited too.
ALTER TABLE lurkers ADD COLUMN invited_at INTEGER;
";

const SCHEMA_V7: &str = "
-- The first invites went out at the same moment as the kick from the room,
-- and the Matrix bridge withdrew them. Empty again, so everyone in their
-- room is kicked once more and invited back after a wait.
UPDATE lurkers SET invited_at = NULL;
";

const SCHEMA_V8: &str = "
-- The nudge list: people admins asked lurker to remind to chat, apart from
-- the lurkers. A row stays after the nudge ends, so admins can see how it
-- went.
CREATE TABLE nudges (
    id INTEGER PRIMARY KEY,
    -- The nick the admin gave, updated when lurker sees them change nick.
    nick TEXT NOT NULL,
    -- The nick folded with IRC's case mapping, to look them up.
    nick_key TEXT NOT NULL,
    -- How many seconds the nudge lasts from added_at, and how many
    -- reminders lurker sends in that time.
    length INTEGER NOT NULL,
    reminders INTEGER NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    -- How many reminders lurker has sent, and when it sent the first and
    -- the last.
    sent INTEGER NOT NULL DEFAULT 0,
    first_reminder_at INTEGER,
    last_reminder_at INTEGER,
    -- When lurker first heard them speak after a reminder.
    spoke_at INTEGER,
    -- When and why the nudge ended, and the admin who ended it, if one did.
    ended_at INTEGER,
    ended TEXT CHECK (ended IN ('spoke', 'finished', 'removed', 'marked')),
    ended_by TEXT
);

-- Each nick has at most one nudge going at a time.
CREATE UNIQUE INDEX nudges_going ON nudges (nick_key) WHERE ended_at IS NULL;
";

/// How often a lurker chatted in the year they were most active.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
}

impl Frequency {
    const ALL: [Frequency; 3] = [Frequency::Daily, Frequency::Weekly, Frequency::Monthly];

    /// The word admins type and the database stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Frequency::Daily => "daily",
            Frequency::Weekly => "weekly",
            Frequency::Monthly => "monthly",
        }
    }

    /// How lurker says it, as in "you used to chat every day in 2022".
    pub fn phrase(self) -> &'static str {
        match self {
            Frequency::Daily => "every day",
            Frequency::Weekly => "every week",
            Frequency::Monthly => "every month",
        }
    }

    pub fn parse(word: &str) -> Option<Frequency> {
        Frequency::ALL
            .into_iter()
            .find(|frequency| frequency.as_str().eq_ignore_ascii_case(word))
    }
}

/// A user an admin marked as a lurker.
#[derive(Debug, Clone, PartialEq)]
pub struct Lurker {
    pub id: i64,
    pub nick: String,
    /// #purgatory for a trusted lurker, #FBI_watchlist for an untrusted one
    pub room: Room,
    /// When they went silent: when they last spoke anywhere, or were first
    /// seen if they never spoke
    pub silent: Option<NaiveDate>,
    /// The year they were most active, and how often they chatted then
    pub peak: Option<(i32, Frequency)>,
    /// When lurker first saw them in their room
    pub here_since: Option<i64>,
    pub marked_by: String,
    pub marked_at: i64,
    /// When lurker next pings them
    pub next_ping_at: Option<i64>,
    /// When lurker last made a fuss about them speaking
    pub fanfare_at: Option<i64>,
    /// How many seconds after their warning they are kicked from lurker's
    /// channels, or None for never
    pub grace: Option<i64>,
    /// When lurker warned them that they would be kicked
    pub warned_at: Option<i64>,
    /// When lurker last saw them say something, in any channel it is in
    pub spoke_at: Option<i64>,
    /// When lurker last invited them to their room
    pub invited_at: Option<i64>,
}

impl Lurker {
    /// When they are kicked from every channel, once they have been warned
    /// and have a grace period.
    pub fn kick_at(&self) -> Option<i64> {
        Some(self.warned_at? + self.grace?)
    }

    /// Whether they have spoken since their warning, which stops the kicks
    /// until they are given a grace period again.
    pub fn released(&self) -> bool {
        matches!((self.warned_at, self.spoke_at), (Some(warned), Some(spoke)) if spoke > warned)
    }
}

/// Someone on the nudge list.
#[derive(Debug, Clone, PartialEq)]
pub struct Nudge {
    pub id: i64,
    pub nick: String,
    /// How many seconds the nudge lasts from when it was added
    pub length: i64,
    /// How many reminders lurker sends in that time
    pub reminders: i64,
    pub added_by: String,
    pub added_at: i64,
    /// How many reminders lurker has sent
    pub sent: i64,
    pub first_reminder_at: Option<i64>,
    /// When lurker first heard them speak after a reminder
    pub spoke_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub ended: Option<NudgeEnd>,
    /// The admin who ended the nudge, if one did
    pub ended_by: Option<String>,
}

impl Nudge {
    /// When the nudge is over.
    pub fn ends_at(&self) -> i64 {
        self.added_at + self.length
    }
}

/// Why a nudge ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NudgeEnd {
    /// They spoke after a reminder.
    Spoke,
    /// Its time ran out.
    Finished,
    /// An admin removed them from the nudge list.
    Removed,
    /// An admin marked them as a lurker.
    Marked,
}

impl NudgeEnd {
    const ALL: [NudgeEnd; 4] = [
        NudgeEnd::Spoke,
        NudgeEnd::Finished,
        NudgeEnd::Removed,
        NudgeEnd::Marked,
    ];

    /// The word the database stores.
    pub fn as_str(self) -> &'static str {
        match self {
            NudgeEnd::Spoke => "spoke",
            NudgeEnd::Finished => "finished",
            NudgeEnd::Removed => "removed",
            NudgeEnd::Marked => "marked",
        }
    }

    fn parse(word: &str) -> Option<NudgeEnd> {
        NudgeEnd::ALL.into_iter().find(|end| end.as_str() == word)
    }
}

/// The columns `load_nudge` reads, in order.
const NUDGE_COLUMNS: &str = "id, nick, length, reminders, added_by, added_at, sent, \
    first_reminder_at, spoke_at, ended_at, ended, ended_by";

/// What marking someone changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Marked {
    New,
    /// They were marked for the other room before.
    Moved,
    /// They were already marked for this room.
    Unchanged,
}

/// What following a nick change did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NickChange {
    NotALurker,
    Followed,
    /// The new nick belongs to another lurker, so nothing changed.
    Taken,
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

    /// The lurker with this nick, if there is one.
    pub fn lurker(&self, nick: &str) -> rusqlite::Result<Option<Lurker>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM lurkers WHERE nick_key = ?1",
                [casemap::fold(nick)],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| self.load(id)).transpose()
    }

    /// Every lurker, by nick.
    pub fn lurkers(&self) -> rusqlite::Result<Vec<Lurker>> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM lurkers ORDER BY nick_key")?;
        let ids = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        ids.into_iter().map(|id| self.load(id)).collect()
    }

    /// Marks someone as a lurker for a room. `here` says whether they are in
    /// that room right now, so their time in it starts counting. Moving a
    /// lurker to the other room starts their time and pings there afresh.
    pub fn mark(
        &self,
        nick: &str,
        room: Room,
        here: bool,
        admin: &str,
        now: i64,
    ) -> rusqlite::Result<Marked> {
        let key = casemap::fold(nick);
        let here_since = here.then_some(now);
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, room FROM lurkers WHERE nick_key = ?1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match existing {
            Some((_, current)) if current == room.as_str() => Ok(Marked::Unchanged),
            Some((id, _)) => {
                self.conn.execute(
                    "UPDATE lurkers
                     SET room = ?2, here_since = ?3, marked_by = ?4, marked_at = ?5,
                         next_ping_at = NULL, warned_at = NULL
                     WHERE id = ?1",
                    params![id, room.as_str(), here_since, admin, now],
                )?;
                self.clear_used_pings(id)?;
                Ok(Marked::Moved)
            }
            None => {
                self.conn.execute(
                    "INSERT INTO lurkers (nick, nick_key, room, here_since, marked_by, marked_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![nick, key, room.as_str(), here_since, admin, now],
                )?;
                Ok(Marked::New)
            }
        }
    }

    /// Deletes a lurker. Returns false when there was none with this nick.
    pub fn forget(&self, nick: &str) -> rusqlite::Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM lurkers WHERE nick_key = ?1",
            [casemap::fold(nick)],
        )?;
        Ok(deleted > 0)
    }

    /// Records when a lurker went silent.
    pub fn set_silent(&self, id: i64, since: NaiveDate) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET silent_since = ?2 WHERE id = ?1",
            params![id, since.to_string()],
        )?;
        Ok(())
    }

    /// Records the year a lurker was most active, and how often they chatted.
    pub fn set_peak(&self, id: i64, year: i32, frequency: Frequency) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET peak_year = ?2, peak_frequency = ?3 WHERE id = ?1",
            params![id, year, frequency.as_str()],
        )?;
        Ok(())
    }

    /// Records that a lurker is in their room, starting their time in it if
    /// this is the first time. Returns true the first time.
    pub fn arrived(&self, id: i64, now: i64) -> rusqlite::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE lurkers SET here_since = ?2 WHERE id = ?1 AND here_since IS NULL",
            params![id, now],
        )?;
        Ok(changed > 0)
    }

    /// Sets when lurker next pings a lurker.
    pub fn set_next_ping(&self, id: i64, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET next_ping_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// The ping messages a lurker has heard since they last heard them all.
    pub fn used_pings(&self, id: i64) -> rusqlite::Result<Vec<usize>> {
        let mut statement = self
            .conn
            .prepare("SELECT template FROM used_pings WHERE lurker_id = ?1")?;
        let used = statement
            .query_map([id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(used.into_iter().map(|template| template as usize).collect())
    }

    /// Records that a lurker heard a ping message.
    pub fn use_ping(&self, id: i64, template: usize) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO used_pings (lurker_id, template) VALUES (?1, ?2)",
            params![id, template as i64],
        )?;
        Ok(())
    }

    /// Starts a lurker's round of ping messages again.
    pub fn clear_used_pings(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM used_pings WHERE lurker_id = ?1", [id])?;
        Ok(())
    }

    /// Sets how long after their warning a lurker is kicked, or None for
    /// never. Their warning starts over, so they are warned again before
    /// any kick.
    pub fn set_grace(&self, id: i64, grace: Option<i64>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET grace = ?2, warned_at = NULL WHERE id = ?1",
            params![id, grace],
        )?;
        Ok(())
    }

    /// Records that lurker saw a lurker say something.
    pub fn set_spoke(&self, id: i64, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET spoke_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// Records that lurker invited a lurker to their room.
    pub fn set_invited(&self, id: i64, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET invited_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// Records that lurker warned a lurker.
    pub fn set_warned(&self, id: i64, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET warned_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// The channels admins asked lurker to join, by name.
    pub fn channels(&self) -> rusqlite::Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT channel FROM channels ORDER BY channel_key")?;
        let channels = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(channels)
    }

    /// Adds a channel for lurker to join. Returns false when it was already
    /// one of them.
    pub fn add_channel(&self, channel: &str, admin: &str, now: i64) -> rusqlite::Result<bool> {
        let added = self.conn.execute(
            "INSERT INTO channels (channel_key, channel, added_by, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (channel_key) DO NOTHING",
            params![casemap::fold(channel), channel, admin, now],
        )?;
        Ok(added > 0)
    }

    /// Removes a channel lurker joins. Returns false when it wasn't one of
    /// them.
    pub fn drop_channel(&self, channel: &str) -> rusqlite::Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM channels WHERE channel_key = ?1",
            [casemap::fold(channel)],
        )?;
        Ok(deleted > 0)
    }

    /// Records that lurker made a fuss about a lurker speaking.
    pub fn set_fanfare(&self, id: i64, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE lurkers SET fanfare_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// The nudge going for a nick, if there is one.
    pub fn nudge(&self, nick: &str) -> rusqlite::Result<Option<Nudge>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {NUDGE_COLUMNS} FROM nudges
                     WHERE nick_key = ?1 AND ended_at IS NULL"
                ),
                [casemap::fold(nick)],
                load_nudge,
            )
            .optional()
    }

    /// The whole nudge list: those going, by nick, then those that ended,
    /// newest first.
    pub fn nudges(&self) -> rusqlite::Result<Vec<Nudge>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {NUDGE_COLUMNS} FROM nudges
             ORDER BY ended_at IS NOT NULL, ended_at DESC, nick_key, id DESC"
        ))?;
        statement.query_map([], load_nudge)?.collect()
    }

    /// Puts someone on the nudge list. Returns false when they already
    /// have a nudge going.
    pub fn add_nudge(
        &self,
        nick: &str,
        length: i64,
        reminders: i64,
        admin: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        if self.nudge(nick)?.is_some() {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO nudges (nick, nick_key, length, reminders, added_by, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![nick, casemap::fold(nick), length, reminders, admin, now],
        )?;
        Ok(true)
    }

    /// Ends the nudge going for a nick, and returns it as it was, or None
    /// when there was none.
    pub fn end_nudge(
        &self,
        nick: &str,
        end: NudgeEnd,
        admin: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<Option<Nudge>> {
        let Some(nudge) = self.nudge(nick)? else {
            return Ok(None);
        };
        self.conn.execute(
            "UPDATE nudges SET ended_at = ?2, ended = ?3, ended_by = ?4 WHERE id = ?1",
            params![nudge.id, now, end.as_str(), admin],
        )?;
        Ok(Some(nudge))
    }

    /// Follows a nick change, if the old nick is a lurker's. A nudge going
    /// for the old nick follows it too, unless the new nick has one.
    pub fn rename(&self, old: &str, new: &str) -> rusqlite::Result<NickChange> {
        let (old_key, new_key) = (casemap::fold(old), casemap::fold(new));
        self.conn.execute(
            "UPDATE OR IGNORE nudges SET nick = ?2, nick_key = ?3
             WHERE nick_key = ?1 AND ended_at IS NULL",
            params![old_key, new, new_key],
        )?;
        if old_key != new_key && self.lurker(new)?.is_some() {
            return Ok(if self.lurker(old)?.is_some() {
                NickChange::Taken
            } else {
                NickChange::NotALurker
            });
        }

        let changed = self.conn.execute(
            "UPDATE lurkers SET nick = ?2, nick_key = ?3 WHERE nick_key = ?1",
            params![old_key, new, new_key],
        )?;
        Ok(if changed > 0 {
            NickChange::Followed
        } else {
            NickChange::NotALurker
        })
    }

    fn load(&self, id: i64) -> rusqlite::Result<Lurker> {
        self.conn.query_row(
            "SELECT nick, room, peak_year, peak_frequency, here_since, marked_by, marked_at,
                    next_ping_at, fanfare_at, grace, warned_at, spoke_at, silent_since,
                    invited_at
             FROM lurkers WHERE id = ?1",
            [id],
            |row| {
                let room: String = row.get(1)?;
                let peak_year: Option<i32> = row.get(2)?;
                let peak_frequency: Option<String> = row.get(3)?;
                let silent: Option<String> = row.get(12)?;
                Ok(Lurker {
                    id,
                    nick: row.get(0)?,
                    // The schema only allows the two rooms' names
                    room: Room::parse(&room).unwrap_or(Room::Purgatory),
                    silent: silent
                        .and_then(|since| NaiveDate::parse_from_str(&since, "%Y-%m-%d").ok()),
                    peak: peak_year.zip(peak_frequency.as_deref().and_then(Frequency::parse)),
                    here_since: row.get(4)?,
                    marked_by: row.get(5)?,
                    marked_at: row.get(6)?,
                    next_ping_at: row.get(7)?,
                    fanfare_at: row.get(8)?,
                    grace: row.get(9)?,
                    warned_at: row.get(10)?,
                    spoke_at: row.get(11)?,
                    invited_at: row.get(13)?,
                })
            },
        )
    }
}

/// A nudge from a row with `NUDGE_COLUMNS`.
fn load_nudge(row: &rusqlite::Row) -> rusqlite::Result<Nudge> {
    let ended: Option<String> = row.get(10)?;
    Ok(Nudge {
        id: row.get(0)?,
        nick: row.get(1)?,
        length: row.get(2)?,
        reminders: row.get(3)?,
        added_by: row.get(4)?,
        added_at: row.get(5)?,
        sent: row.get(6)?,
        first_reminder_at: row.get(7)?,
        spoke_at: row.get(8)?,
        ended_at: row.get(9)?,
        // The schema only allows the four reasons
        ended: ended.as_deref().and_then(NudgeEnd::parse),
        ended_by: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

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
    fn marks_and_finds_lurkers_regardless_of_case() {
        let db = Db::open_in_memory();
        assert_eq!(
            db.mark("Alice", Room::Purgatory, false, "rachel", 100)
                .unwrap(),
            Marked::New
        );

        let alice = db.lurker("ALICE").unwrap().unwrap();
        assert_eq!(alice.nick, "Alice");
        assert_eq!(alice.room, Room::Purgatory);
        assert_eq!(alice.here_since, None);
        assert_eq!(alice.marked_by, "rachel");
        assert_eq!(db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn upgrades_a_version_1_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{SCHEMA_V1} PRAGMA user_version = 1;"))
            .unwrap();
        conn.execute(
            "INSERT INTO lurkers (nick, nick_key, room, marked_by, marked_at)
             VALUES ('alice', 'alice', 'purgatory', 'rachel', 100)",
            [],
        )
        .unwrap();

        let db = Db::setup(conn).unwrap();
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.next_ping_at, None);
        assert_eq!(alice.fanfare_at, None);
        assert_eq!((alice.warned_at, alice.spoke_at), (None, None));
        assert_eq!(alice.silent, None);
    }

    #[test]
    fn keeps_the_most_recent_channel_date_as_the_silent_date() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "PRAGMA foreign_keys = ON; {SCHEMA_V1} {SCHEMA_V2} {SCHEMA_V3} {SCHEMA_V4} \
             PRAGMA user_version = 4;"
        ))
        .unwrap();
        conn.execute_batch(
            "INSERT INTO lurkers (id, nick, nick_key, room, marked_by, marked_at)
             VALUES (1, 'alice', 'alice', 'purgatory', 'rachel', 100),
                    (2, 'bob', 'bob', 'watchlist', 'rachel', 100);
             INSERT INTO lurker_channels (lurker_id, channel, channel_key, since)
             VALUES (1, '#wetfish', '#wetfish', '2024-11-19'),
                    (1, '#dryfish', '#dryfish', '2025-11-19'),
                    (1, '#music', '#music', '2025-01-01');",
        )
        .unwrap();

        let db = Db::setup(conn).unwrap();
        assert_eq!(
            db.lurker("alice").unwrap().unwrap().silent,
            Some(date("2025-11-19"))
        );
        assert_eq!(db.lurker("bob").unwrap().unwrap().silent, None);
        let tables: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'lurker_channels'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
    }

    #[test]
    fn upgrades_a_version_5_database_with_nobody_invited() {
        let conn = Connection::open_in_memory().unwrap();
        let earlier: String = MIGRATIONS[..5].concat();
        conn.execute_batch(&format!("{earlier} PRAGMA user_version = 5;"))
            .unwrap();
        conn.execute(
            "INSERT INTO lurkers (nick, nick_key, room, marked_by, marked_at, here_since)
             VALUES ('bob', 'bob', 'watchlist', 'rachel', 100, 200)",
            [],
        )
        .unwrap();

        let db = Db::setup(conn).unwrap();
        let bob = db.lurker("bob").unwrap().unwrap();
        assert_eq!((bob.here_since, bob.invited_at), (Some(200), None));

        db.set_invited(bob.id, 300).unwrap();
        assert_eq!(db.lurker("bob").unwrap().unwrap().invited_at, Some(300));
    }

    #[test]
    fn upgrading_a_version_6_database_empties_the_invites() {
        let conn = Connection::open_in_memory().unwrap();
        let earlier: String = MIGRATIONS[..6].concat();
        conn.execute_batch(&format!("{earlier} PRAGMA user_version = 6;"))
            .unwrap();
        conn.execute(
            "INSERT INTO lurkers (nick, nick_key, room, marked_by, marked_at, invited_at)
             VALUES ('bob', 'bob', 'watchlist', 'rachel', 100, 300)",
            [],
        )
        .unwrap();

        let db = Db::setup(conn).unwrap();
        assert_eq!(db.lurker("bob").unwrap().unwrap().invited_at, None);
    }

    #[test]
    fn keeps_the_nudge_list() {
        let db = Db::open_in_memory();
        assert!(db.add_nudge("Alice", 1000, 4, "rachel", 100).unwrap());
        assert!(!db.add_nudge("alice", 2000, 2, "sam", 200).unwrap());
        assert!(db.add_nudge("bob", 1000, 1, "rachel", 100).unwrap());

        let alice = db.nudge("ALICE").unwrap().unwrap();
        assert_eq!(
            (alice.nick.as_str(), alice.length, alice.reminders),
            ("Alice", 1000, 4)
        );
        assert_eq!((alice.sent, alice.ended), (0, None));
        assert_eq!(alice.ends_at(), 1100);

        let ended = db
            .end_nudge("alice", NudgeEnd::Removed, Some("sam"), 300)
            .unwrap()
            .unwrap();
        assert_eq!(ended.id, alice.id);
        assert_eq!(db.nudge("alice").unwrap(), None);
        assert_eq!(
            db.end_nudge("alice", NudgeEnd::Removed, None, 400).unwrap(),
            None
        );

        // A new nudge for the same nick, with the old one kept
        assert!(db.add_nudge("alice", 500, 1, "rachel", 500).unwrap());
        let list: Vec<(String, Option<NudgeEnd>)> = db
            .nudges()
            .unwrap()
            .into_iter()
            .map(|nudge| (nudge.nick, nudge.ended))
            .collect();
        assert_eq!(
            list,
            [
                ("alice".to_owned(), None),
                ("bob".to_owned(), None),
                ("Alice".to_owned(), Some(NudgeEnd::Removed)),
            ]
        );
        let old = &db.nudges().unwrap()[2];
        assert_eq!(
            (old.ended_at, old.ended_by.as_deref()),
            (Some(300), Some("sam"))
        );
    }

    #[test]
    fn nudges_follow_nick_changes() {
        let db = Db::open_in_memory();
        db.add_nudge("alice", 1000, 4, "rachel", 100).unwrap();
        db.add_nudge("bob", 1000, 4, "rachel", 100).unwrap();

        assert_eq!(
            db.rename("alice", "alice_").unwrap(),
            NickChange::NotALurker
        );
        assert_eq!(db.nudge("alice_").unwrap().unwrap().nick, "alice_");
        assert_eq!(db.nudge("alice").unwrap(), None);

        // Not onto a nick that has a nudge going
        db.rename("alice_", "bob").unwrap();
        assert_eq!(db.nudge("alice_").unwrap().unwrap().nick, "alice_");

        // Ended nudges keep the nick they had
        db.end_nudge("bob", NudgeEnd::Finished, None, 200).unwrap();
        db.rename("bob", "bobby").unwrap();
        assert_eq!(db.nudge("bobby").unwrap(), None);
    }

    #[test]
    fn remembers_grace_and_warnings() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        assert_eq!(db.lurker("alice").unwrap().unwrap().kick_at(), None);

        db.set_grace(id, Some(3600)).unwrap();
        assert_eq!(db.lurker("alice").unwrap().unwrap().kick_at(), None);
        db.set_warned(id, 1000).unwrap();
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!((alice.grace, alice.warned_at), (Some(3600), Some(1000)));
        assert_eq!(alice.kick_at(), Some(4600));

        // A new grace period starts the warning over
        db.set_grace(id, Some(60)).unwrap();
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!((alice.grace, alice.warned_at), (Some(60), None));

        db.set_grace(id, None).unwrap();
        assert_eq!(db.lurker("alice").unwrap().unwrap().kick_at(), None);
    }

    #[test]
    fn speaking_after_the_warning_releases_lurkers() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        db.set_grace(id, Some(3600)).unwrap();

        // Speaking before any warning changes nothing
        db.set_spoke(id, 900).unwrap();
        db.set_warned(id, 1000).unwrap();
        assert!(!db.lurker("alice").unwrap().unwrap().released());

        db.set_spoke(id, 1100).unwrap();
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.spoke_at, Some(1100));
        assert!(alice.released());

        // A new grace period starts again
        db.set_grace(id, Some(60)).unwrap();
        assert!(!db.lurker("alice").unwrap().unwrap().released());
    }

    #[test]
    fn keeps_the_channels_to_join() {
        let db = Db::open_in_memory();
        assert!(db.add_channel("#Wetfish", "rachel", 100).unwrap());
        assert!(!db.add_channel("#wetfish", "rachel", 100).unwrap());
        assert!(db.add_channel("#music", "rachel", 100).unwrap());
        assert_eq!(db.channels().unwrap(), ["#music", "#Wetfish"]);

        assert!(db.drop_channel("#WETFISH").unwrap());
        assert!(!db.drop_channel("#wetfish").unwrap());
        assert_eq!(db.channels().unwrap(), ["#music"]);
    }

    #[test]
    fn remembers_pings_and_fanfare() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, true, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;

        db.set_next_ping(id, 500).unwrap();
        db.set_fanfare(id, 600).unwrap();
        db.use_ping(id, 3).unwrap();
        db.use_ping(id, 1).unwrap();
        db.use_ping(id, 3).unwrap();

        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.next_ping_at, Some(500));
        assert_eq!(alice.fanfare_at, Some(600));
        let mut used = db.used_pings(id).unwrap();
        used.sort();
        assert_eq!(used, [1, 3]);

        db.clear_used_pings(id).unwrap();
        assert!(db.used_pings(id).unwrap().is_empty());
    }

    #[test]
    fn moving_rooms_starts_pings_afresh() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, true, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        db.set_next_ping(id, 500).unwrap();
        db.use_ping(id, 2).unwrap();

        db.set_warned(id, 150).unwrap();

        db.mark("alice", Room::Watchlist, false, "rachel", 200)
            .unwrap();
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!((alice.next_ping_at, alice.warned_at), (None, None));
        assert!(db.used_pings(id).unwrap().is_empty());
    }

    #[test]
    fn marking_again_moves_or_keeps() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, true, "rachel", 100)
            .unwrap();
        assert_eq!(
            db.mark("alice", Room::Purgatory, false, "rachel", 200)
                .unwrap(),
            Marked::Unchanged
        );
        assert_eq!(db.lurker("alice").unwrap().unwrap().here_since, Some(100));

        assert_eq!(
            db.mark("alice", Room::Watchlist, false, "sam", 300)
                .unwrap(),
            Marked::Moved
        );
        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.room, Room::Watchlist);
        assert_eq!(alice.here_since, None);
        assert_eq!(alice.marked_by, "sam");
    }

    #[test]
    fn starts_time_in_the_room_once() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        assert!(db.arrived(id, 200).unwrap());
        assert!(!db.arrived(id, 300).unwrap());
        assert_eq!(db.lurker("alice").unwrap().unwrap().here_since, Some(200));
    }

    #[test]
    fn keeps_the_silent_date_and_peak() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        assert_eq!(db.lurker("alice").unwrap().unwrap().silent, None);

        db.set_silent(id, date("2024-11-19")).unwrap();
        db.set_silent(id, date("2025-01-01")).unwrap();
        db.set_peak(id, 2022, Frequency::Daily).unwrap();

        let alice = db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.silent, Some(date("2025-01-01")));
        assert_eq!(alice.peak, Some((2022, Frequency::Daily)));
    }

    #[test]
    fn follows_nick_changes() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        db.mark("bob", Room::Watchlist, false, "rachel", 100)
            .unwrap();

        assert_eq!(db.rename("alice", "alice_").unwrap(), NickChange::Followed);
        assert_eq!(db.lurker("alice_").unwrap().unwrap().nick, "alice_");
        assert_eq!(db.lurker("alice").unwrap(), None);

        assert_eq!(db.rename("alice_", "Alice_").unwrap(), NickChange::Followed);
        assert_eq!(db.rename("alice_", "bob").unwrap(), NickChange::Taken);
        assert_eq!(db.rename("carol", "bob").unwrap(), NickChange::NotALurker);
        assert_eq!(db.rename("carol", "dave").unwrap(), NickChange::NotALurker);
    }

    #[test]
    fn forgets_lurkers_and_their_pings() {
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        let id = db.lurker("alice").unwrap().unwrap().id;
        db.use_ping(id, 2).unwrap();

        assert!(db.forget("Alice").unwrap());
        assert!(!db.forget("alice").unwrap());
        assert_eq!(db.lurker("alice").unwrap(), None);
        let pings: i64 = db
            .conn
            .query_row("SELECT count(*) FROM used_pings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pings, 0);
    }

    #[test]
    fn lists_lurkers_by_nick() {
        let db = Db::open_in_memory();
        db.mark("carol", Room::Purgatory, false, "rachel", 100)
            .unwrap();
        db.mark("Alice", Room::Watchlist, false, "rachel", 100)
            .unwrap();
        let nicks: Vec<String> = db.lurkers().unwrap().into_iter().map(|l| l.nick).collect();
        assert_eq!(nicks, ["Alice", "carol"]);
    }
}
