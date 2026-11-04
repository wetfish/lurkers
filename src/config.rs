//! lurker's settings, read from a TOML file laid out like
//! `lurker.example.toml`.

use std::fs;
use std::net::SocketAddr;
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
    #[serde(default)]
    pub http: Http,
    pub oper: Option<Oper>,
    #[serde(default)]
    pub admins: Vec<Admin>,
}

/// How to reach the IRC server and who lurker is there. lurker always
/// connects over TLS.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rooms {
    pub purgatory: String,
    pub watchlist: String,
}

/// Which of the two rooms. The database stores this rather than the
/// channel name, so renaming a room in the settings keeps its lurkers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Room {
    Purgatory,
    Watchlist,
}

/// Where lurker keeps the lurkers admins have marked.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub path: PathBuf,
}

/// The status page, which says only whether lurker is connected to IRC.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http {
    /// The address and port to serve it on, such as `0.0.0.0:8080`. No page
    /// is served without one.
    pub listen: Option<String>,
}

/// lurker's IRC operator login, which lets it op itself, kick lurkers from
/// every channel and sajoin them into their room.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oper {
    pub name: String,
    pub password: String,
}

impl std::fmt::Debug for Oper {
    /// Leaves the password out, so it never reaches a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oper")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Someone who may give lurker commands.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    pub nick: String,
    /// The hosts they connect from. `*` matches any characters and `?` any
    /// one character.
    pub hosts: Vec<String>,
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

    /// The room a name from the database stands for.
    pub fn parse(name: &str) -> Option<Room> {
        Room::ALL.into_iter().find(|room| room.as_str() == name)
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
    /// The settings in `lurker.example.toml`, for tests.
    #[cfg(test)]
    pub fn example() -> Config {
        Config::parse(include_str!("../lurker.example.toml")).unwrap()
    }

    /// The admin with this nick connecting from this host, if there is one.
    pub fn admin(&self, nick: &str, host: &str) -> Option<&Admin> {
        self.admins.iter().find(|admin| {
            casemap::eq(&admin.nick, nick)
                && admin.hosts.iter().any(|pattern| wildcard(pattern, host))
        })
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

        if let Some(oper) = &self.oper {
            check_name("oper.name", &oper.name)?;
            if oper.password.is_empty() || oper.password.contains(['\r', '\n', '\0']) {
                return Err("oper.password must be one line of text".to_owned());
            }
        }

        if let Some(listen) = &self.http.listen
            && listen.parse::<SocketAddr>().is_err()
        {
            return Err(format!(
                "http.listen is {listen:?}, which must be an address and port, such as \
                 0.0.0.0:8080"
            ));
        }

        for admin in &self.admins {
            check_name("admins.nick", &admin.nick)?;
            let nick = &admin.nick;
            if admin.hosts.is_empty() {
                return Err(format!("admin {nick} needs at least one host in hosts"));
            }
            for host in &admin.hosts {
                if host.is_empty() || host.contains(|c: char| c.is_whitespace() || "!@".contains(c))
                {
                    return Err(format!(
                        "admin {nick} has the host {host:?}, which must be a host name or \
                         pattern without spaces, ! or @"
                    ));
                }
                if host.chars().all(|c| "*?.".contains(c)) {
                    return Err(format!(
                        "admin {nick} has the host {host:?}, which would let anyone using \
                         the nick give lurker commands"
                    ));
                }
            }
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

/// Whether a host matches a pattern, ignoring case, where `*` matches any
/// characters and `?` any one character.
fn wildcard(pattern: &str, host: &str) -> bool {
    let pattern: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let host: Vec<char> = host.to_ascii_lowercase().chars().collect();

    let (mut p, mut h) = (0, 0);
    // Where the last `*` was, and the host position it is matching up to
    let mut star: Option<(usize, usize)> = None;

    while h < host.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == host[h]) {
            p += 1;
            h += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, h));
            p += 1;
        } else if let Some((star_p, star_h)) = star {
            // Let the last `*` match one more character and try again
            p = star_p + 1;
            h = star_h + 1;
            star = Some((star_p, star_h + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// The settings file and the settings lurker is running with. While lurker
/// runs, it reads the file again when it changes and takes the new admins;
/// other settings take effect when lurker restarts.
pub struct Settings {
    path: PathBuf,
    /// The file as last read, to notice when it changes. None when it could
    /// not be read.
    text: Option<String>,
    pub config: Config,
}

impl Settings {
    /// Reads and checks the settings file.
    pub fn load(path: &Path) -> Result<Settings, String> {
        let text = read(path)?;
        let config = Config::parse(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Settings {
            path: path.to_owned(),
            text: Some(text),
            config,
        })
    }

    /// Settings from `lurker.example.toml`, for tests.
    #[cfg(test)]
    pub fn example() -> Settings {
        Settings {
            path: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/lurker.example.toml")),
            text: None,
            config: Config::example(),
        }
    }

    /// Reads the file again if it changed since it was last read, or always
    /// when `force` is set. Returns what happened, for the log or the admin
    /// who asked, or None when nothing changed.
    pub fn reload(&mut self, force: bool) -> Option<Result<String, String>> {
        let text = match read(&self.path) {
            Ok(text) => text,
            Err(err) => {
                // Say so once, rather than every time lurker checks
                let was_readable = self.text.take().is_some();
                return (force || was_readable).then_some(Err(err));
            }
        };

        if !force && self.text.as_deref() == Some(text.as_str()) {
            return None;
        }
        let result = self.apply(&text);
        self.text = Some(text);
        Some(result)
    }

    /// Takes the admins from the file's text, keeping the current settings
    /// if anything in it is wrong.
    fn apply(&mut self, text: &str) -> Result<String, String> {
        let path = self.path.display();
        let new = Config::parse(text)
            .map_err(|err| format!("{path}: {err}. lurker is keeping its previous settings"))?;

        let count = new.admins.len();
        let mut summary = format!(
            "Read {path} again: {count} admin{}",
            if count == 1 { "" } else { "s" }
        );
        if new.irc != self.config.irc
            || new.rooms != self.config.rooms
            || new.database != self.config.database
            || new.http != self.config.http
            || new.oper != self.config.oper
        {
            summary.push_str(
                ". Changes to [irc], [rooms], [database], [http] and [oper] take effect when \
                 lurker restarts",
            );
        }

        self.config.admins = new.admins;
        Ok(summary)
    }
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "[irc]\nserver = \"irc.example.net\"\n";

    fn with_admin(hosts: &str) -> String {
        format!("{MINIMAL}[[admins]]\nnick = \"rachel\"\nhosts = {hosts}\n")
    }

    #[test]
    fn example_file_is_valid() {
        let config = Config::example();
        assert_eq!(config.irc.nick, "lurker");
        assert_eq!(config.rooms.all(), ["#purgatory", "#FBI_watchlist"]);
        assert_eq!(config.admins.len(), 1);
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
        assert!(config.admins.is_empty());
        assert_eq!(config.http.listen, None);
        assert!(config.oper.is_none());
    }

    #[test]
    fn reads_the_oper_login_without_showing_the_password() {
        let text = format!("{MINIMAL}[oper]\nname = \"lurker\"\npassword = \"hunter2 x\"\n");
        let config = Config::parse(&text).unwrap();
        let oper = config.oper.unwrap();
        assert_eq!(
            (oper.name.as_str(), oper.password.as_str()),
            ("lurker", "hunter2 x")
        );
        assert!(!format!("{oper:?}").contains("hunter2"));

        for (name, password) in [("", "x"), ("lurk er", "x"), ("lurker", "")] {
            let text = format!("{MINIMAL}[oper]\nname = \"{name}\"\npassword = \"{password}\"\n");
            assert!(
                Config::parse(&text).is_err(),
                "accepted {name:?} {password:?}"
            );
        }
    }

    #[test]
    fn checks_the_status_page_address() {
        let text = format!("{MINIMAL}[http]\nlisten = \"0.0.0.0:8080\"\n");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.http.listen.as_deref(), Some("0.0.0.0:8080"));

        for listen in ["8080", "localhost:8080", "0.0.0.0"] {
            let text = format!("{MINIMAL}[http]\nlisten = \"{listen}\"\n");
            let err = Config::parse(&text).unwrap_err();
            assert!(err.contains("http.listen"), "{err}");
        }
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

    #[test]
    fn rejects_admins_anyone_could_be() {
        for hosts in ["[]", "[\"*\"]", "[\"*.*\"]", "[\"\"]", "[\"user@host\"]"] {
            assert!(
                Config::parse(&with_admin(hosts)).is_err(),
                "accepted hosts {hosts}"
            );
        }
    }

    #[test]
    fn recognizes_admins_by_nick_and_host() {
        let config = Config::parse(&with_admin("[\"*.wetfish.net\", \"Fish-??.chat\"]")).unwrap();
        assert!(config.admin("Rachel", "home.wetfish.net").is_some());
        assert!(config.admin("rachel", "FISH-12.chat").is_some());
        assert!(config.admin("rachel", "fish-123.chat").is_none());
        assert!(config.admin("rachel", "wetfish.net.example.com").is_none());
        assert!(config.admin("bob", "home.wetfish.net").is_none());
    }

    #[test]
    fn matches_wildcards() {
        assert!(wildcard("*", ""));
        assert!(wildcard("a*b*c", "aXXbYYc"));
        assert!(wildcard("a*b", "abab"));
        assert!(!wildcard("a*b", "abac"));
        assert!(wildcard("??", "ab"));
        assert!(!wildcard("??", "abc"));
        assert!(wildcard("host", "HOST"));
    }

    #[test]
    fn reload_takes_new_admins() {
        let mut settings = Settings::example();
        let summary = settings.apply(&with_admin("[\"new.host\"]")).unwrap();
        assert!(
            summary.ends_with("lurker.example.toml again: 1 admin"),
            "{summary}"
        );
        assert_eq!(settings.config.admins[0].hosts, ["new.host"]);
    }

    #[test]
    fn reload_says_other_changes_wait_for_a_restart() {
        let mut settings = Settings::example();
        let text = "[irc]\nserver = \"irc.example.net\"\nnick = \"lurker2\"\n";
        let summary = settings.apply(text).unwrap();
        assert!(summary.contains("0 admins"), "{summary}");
        assert!(summary.contains("restarts"), "{summary}");
        assert_eq!(settings.config.irc.nick, "lurker");
        assert!(settings.config.admins.is_empty());
    }

    #[test]
    fn reload_keeps_the_settings_when_the_file_is_wrong() {
        let mut settings = Settings::example();
        let err = settings.apply("[irc]\n").unwrap_err();
        assert!(err.contains("keeping its previous settings"), "{err}");
        assert_eq!(settings.config.admins.len(), 1);
    }

    #[test]
    fn reload_without_other_changes_says_nothing_about_restarting() {
        let mut settings = Settings::example();
        let summary = settings
            .apply(include_str!("../lurker.example.toml"))
            .unwrap();
        assert!(!summary.contains("restarts"), "{summary}");
    }
}
