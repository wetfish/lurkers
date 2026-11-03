//! lurker's settings, read from a TOML file laid out like
//! `lurker.example.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::casemap;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub irc: Irc,
    #[serde(default)]
    pub rooms: Rooms,
    #[serde(default)]
    pub database: Database,
}

/// How to reach the IRC server and who lurker is there. lurker always
/// connects over TLS.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Irc {
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_nick")]
    pub nick: String,
    username: Option<String>,
    realname: Option<String>,
}

/// The two rooms lurker keeps watch over.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rooms {
    pub purgatory: String,
    pub watchlist: String,
}

/// Which of the two rooms. The database stores this rather than the
/// channel name, so renaming a room in the settings keeps its users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Room {
    Purgatory,
    Watchlist,
}

/// Where lurker keeps what it knows about the users in its rooms.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub path: PathBuf,
}

fn default_port() -> u16 {
    6697
}

fn default_nick() -> String {
    "lurker".to_owned()
}

impl Default for Rooms {
    fn default() -> Self {
        Rooms {
            purgatory: "#purgatory".to_owned(),
            watchlist: "#FBI_watchlist".to_owned(),
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Database {
            path: PathBuf::from("lurker.sqlite"),
        }
    }
}

impl Room {
    pub const ALL: [Room; 2] = [Room::Purgatory, Room::Watchlist];

    /// The name the database stores for the room.
    pub fn as_str(self) -> &'static str {
        match self {
            Room::Purgatory => "purgatory",
            Room::Watchlist => "watchlist",
        }
    }
}

impl Irc {
    /// The username (ident), which defaults to the nick.
    pub fn username(&self) -> &str {
        self.username.as_deref().unwrap_or(&self.nick)
    }

    /// The real name shown in /whois, which defaults to "lurker".
    pub fn realname(&self) -> &str {
        self.realname.as_deref().unwrap_or("lurker")
    }
}

impl Rooms {
    /// Both rooms, #purgatory first.
    pub fn all(&self) -> [&str; 2] {
        Room::ALL.map(|room| self.channel(room))
    }

    /// The channel name of a room.
    pub fn channel(&self, room: Room) -> &str {
        match room {
            Room::Purgatory => &self.purgatory,
            Room::Watchlist => &self.watchlist,
        }
    }

    /// Which room a channel is, if it is one of them.
    pub fn find(&self, channel: &str) -> Option<Room> {
        Room::ALL
            .into_iter()
            .find(|&room| casemap::eq(self.channel(room), channel))
    }
}

impl Config {
    /// Reads and checks the settings file.
    pub fn load(path: &Path) -> Result<Config, String> {
        let text =
            fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
        Config::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
    }

    /// The settings in `lurker.example.toml`, for tests.
    #[cfg(test)]
    pub fn example() -> Config {
        Config::parse(include_str!("../lurker.example.toml")).unwrap()
    }

    fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|err| err.to_string())?;
        config.check()?;
        Ok(config)
    }

    /// Catches settings the server would reject, before connecting.
    fn check(&self) -> Result<(), String> {
        let irc = &self.irc;

        if irc.server.is_empty() || irc.server.contains(char::is_whitespace) {
            return Err("irc.server must be a host name, such as irc.example.net".to_owned());
        }
        if irc.port == 0 {
            return Err("irc.port must be a port number from 1 to 65535".to_owned());
        }
        check_name("irc.nick", &irc.nick)?;
        if irc
            .nick
            .starts_with(|c: char| c.is_ascii_digit() || "#&-".contains(c))
        {
            return Err("irc.nick must not start with a digit, #, & or -".to_owned());
        }
        if let Some(username) = &irc.username {
            check_name("irc.username", username)?;
        }
        if let Some(realname) = &irc.realname
            && (realname.is_empty() || realname.contains(['\r', '\n', '\0']))
        {
            return Err("irc.realname must be one line of text".to_owned());
        }

        check_room("rooms.purgatory", &self.rooms.purgatory)?;
        check_room("rooms.watchlist", &self.rooms.watchlist)?;
        if casemap::eq(&self.rooms.purgatory, &self.rooms.watchlist) {
            return Err("rooms.purgatory and rooms.watchlist must be different rooms".to_owned());
        }

        if self.database.path.as_os_str().is_empty() {
            return Err("database.path must not be empty".to_owned());
        }

        Ok(())
    }
}

/// A nick or username: one word with no characters IRC uses as separators.
fn check_name(setting: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{setting} must not be empty"));
    }
    if value.contains(|c: char| c.is_whitespace() || c.is_control() || ",:!@*?".contains(c)) {
        return Err(format!(
            "{setting} must be one word without spaces or any of , : ! @ * ?"
        ));
    }
    Ok(())
}

fn check_room(setting: &str, value: &str) -> Result<(), String> {
    if !value.starts_with('#') || value.len() < 2 {
        return Err(format!("{setting} must be a channel name starting with #"));
    }
    if value.contains(|c: char| c.is_whitespace() || c.is_control() || c == ',') {
        return Err(format!("{setting} must not contain spaces or commas"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "[irc]\nserver = \"irc.example.net\"\n";

    #[test]
    fn example_file_is_valid() {
        let config = Config::example();
        assert_eq!(config.irc.nick, "lurker");
        assert_eq!(config.rooms.all(), ["#purgatory", "#FBI_watchlist"]);
    }

    #[test]
    fn fills_in_defaults() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.irc.port, 6697);
        assert_eq!(config.irc.nick, "lurker");
        assert_eq!(config.irc.username(), "lurker");
        assert_eq!(config.irc.realname(), "lurker");
        assert_eq!(config.rooms.all(), ["#purgatory", "#FBI_watchlist"]);
        assert_eq!(config.database.path, PathBuf::from("lurker.sqlite"));
    }

    #[test]
    fn finds_rooms_regardless_of_case() {
        let config = Config::example();
        assert_eq!(config.rooms.find("#PURGATORY"), Some(Room::Purgatory));
        assert_eq!(config.rooms.find("#fbi_watchlist"), Some(Room::Watchlist));
        assert_eq!(config.rooms.find("#wetfish"), None);
    }

    #[test]
    fn username_defaults_to_the_nick() {
        let text = format!("{MINIMAL}nick = \"lurker-test\"\n");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.irc.username(), "lurker-test");
    }

    #[test]
    fn requires_a_server() {
        let err = Config::parse("[irc]\nnick = \"lurker\"\n").unwrap_err();
        assert!(err.contains("server"), "{err}");
    }

    #[test]
    fn rejects_unknown_settings() {
        let text = format!("{MINIMAL}tls = false\n");
        let err = Config::parse(&text).unwrap_err();
        assert!(err.contains("tls"), "{err}");
    }

    #[test]
    fn rejects_bad_nicks() {
        for nick in ["", "lurk er", "1lurker", "#lurker", "lurker!"] {
            let text = format!("{MINIMAL}nick = \"{nick}\"\n");
            assert!(Config::parse(&text).is_err(), "accepted nick {nick:?}");
        }
    }

    #[test]
    fn rejects_bad_rooms() {
        for room in ["purgatory", "#", "#two words", "#a,#b"] {
            let text = format!("{MINIMAL}[rooms]\npurgatory = \"{room}\"\nwatchlist = \"#w\"\n");
            assert!(Config::parse(&text).is_err(), "accepted room {room:?}");
        }
    }

    #[test]
    fn rejects_the_same_room_twice() {
        let text =
            format!("{MINIMAL}[rooms]\npurgatory = \"#Lurkers\"\nwatchlist = \"#lurkers\"\n");
        let err = Config::parse(&text).unwrap_err();
        assert!(err.contains("different"), "{err}");
    }
}
