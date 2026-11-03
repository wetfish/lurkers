//! Keeps lurker on IRC: connects over TLS, joins both rooms and rejoins them
//! when needed, takes its nick back when it was taken, reconnects after a
//! lost connection, and quits with a message when lurker is stopped. Along
//! the way it follows who is in the rooms, and answers admins.

use std::collections::HashSet;
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use irc::client::ClientStream;
use irc::client::prelude::{Client, Command, Config as IrcConfig, Message, Prefix, Response};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::admin;
use crate::casemap;
use crate::config::{Room, Settings};
use crate::db::{Db, NickChange};
use crate::presence::Presence;
use crate::shutdown::Shutdown;

/// Sent with QUIT when lurker is stopped, which is usually for a deploy.
pub const QUIT_MESSAGE: &str = "brb, getting a software update";

/// How long connecting, including the TLS handshake, may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the server to close the connection after QUIT.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to join the rooms lurker is not in, such as after a kick.
const REJOIN_INTERVAL: Duration = Duration::from_secs(60);

/// How often to check whether the settings file changed.
const SETTINGS_INTERVAL: Duration = Duration::from_secs(30);

/// The wait before reconnecting, doubled after each attempt up to
/// `MAX_RETRY_DELAY`.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// A connection that lasted this long counts as having worked, so the wait
/// before reconnecting starts again from `FIRST_RETRY_DELAY`.
const STABLE_CONNECTION: Duration = Duration::from_secs(300);

/// The channel status prefixes the server puts before nicks in its list of
/// names, such as `@` for an op.
const STATUS_PREFIXES: &[char] = &['~', '&', '@', '%', '+'];

/// Stays connected, reconnecting whenever the connection is lost, until
/// lurker is stopped.
pub async fn run(settings: &mut Settings, db: &Db, shutdown: &mut Shutdown) {
    let mut delay = FIRST_RETRY_DELAY;

    loop {
        let started = Instant::now();

        match connect(settings, db, shutdown).await {
            Ok(()) => return,
            Err(err) => eprintln!("error: {err}"),
        }

        if started.elapsed() >= STABLE_CONNECTION {
            delay = FIRST_RETRY_DELAY;
        }

        eprintln!("Reconnecting in {} seconds", delay.as_secs());
        tokio::select! {
            () = time::sleep(delay) => {}
            () = shutdown.recv() => return,
        }

        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Connects once and handles messages until the connection is lost, which
/// is an error, or lurker is stopped.
async fn connect(settings: &mut Settings, db: &Db, shutdown: &mut Shutdown) -> Result<(), String> {
    let server = settings.config.irc.server.clone();
    eprintln!("Connecting to {server}:{}", settings.config.irc.port);

    let irc = irc_config(settings);
    let connecting = time::timeout(CONNECT_TIMEOUT, Client::from_config(irc));
    let mut client = tokio::select! {
        result = connecting => match result {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => return Err(format!("connecting to {server}: {}", describe(&err))),
            Err(_) => {
                return Err(format!(
                    "connecting to {server}: no answer after {} seconds",
                    CONNECT_TIMEOUT.as_secs()
                ));
            }
        },
        () = shutdown.recv() => return Ok(()),
    };

    client.identify().map_err(|err| describe(&err))?;
    let mut stream = client.stream().map_err(|err| describe(&err))?;
    let mut session = Session::new(settings, db);
    serve(&client, &mut stream, &mut session, shutdown).await
}

/// Handles messages, rejoins rooms and rereads the settings until the
/// connection is lost or lurker is stopped.
async fn serve(
    client: &Client,
    stream: &mut ClientStream,
    session: &mut Session<'_>,
    shutdown: &mut Shutdown,
) -> Result<(), String> {
    let mut rejoin = time::interval(REJOIN_INTERVAL);
    rejoin.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut settings = time::interval(SETTINGS_INTERVAL);
    settings.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let commands = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(message)) => session.handle(&message, unix_now()),
                Some(Err(err)) => return Err(format!("connection lost: {}", describe(&err))),
                None => return Err("the server closed the connection".to_owned()),
            },
            _ = rejoin.tick() => session.rejoin(),
            _ = settings.tick() => {
                session.check_settings();
                Vec::new()
            }
            () = shutdown.recv() => {
                quit(client, stream).await;
                return Ok(());
            }
        };

        for command in commands {
            client.send(command).map_err(|err| describe(&err))?;
        }
    }
}

/// Sends QUIT, then keeps reading until the server closes the connection,
/// since that is what writes the QUIT out.
async fn quit(client: &Client, stream: &mut ClientStream) {
    eprintln!("Quitting: {QUIT_MESSAGE}");
    if client.send_quit(QUIT_MESSAGE).is_err() {
        return;
    }

    let closed = async { while let Some(Ok(_)) = stream.next().await {} };
    let _ = time::timeout(QUIT_TIMEOUT, closed).await;
}

/// The irc crate's settings for one connection. The crate joins the rooms
/// once the server has accepted the connection, answers server pings, and
/// tries the alternative nicks when the nick is taken.
fn irc_config(settings: &Settings) -> IrcConfig {
    let config = &settings.config;
    let nick = &config.irc.nick;

    IrcConfig {
        nickname: Some(nick.clone()),
        alt_nicks: alt_nicks(nick).to_vec(),
        username: Some(config.irc.username().to_owned()),
        realname: Some(config.irc.realname().to_owned()),
        server: Some(config.irc.server.clone()),
        port: Some(config.irc.port),
        use_tls: Some(true),
        channels: config.rooms.all().map(str::to_owned).to_vec(),
        version: Some(format!("lurker {}", env!("CARGO_PKG_VERSION"))),
        source: Some(env!("CARGO_PKG_REPOSITORY").to_owned()),
        ..IrcConfig::default()
    }
}

/// The nicks lurker uses when its own is taken.
fn alt_nicks(nick: &str) -> [String; 2] {
    [format!("{nick}_"), format!("{nick}__")]
}

/// An error and its causes, since the irc crate's own messages, such as
/// "an io error occurred", leave the cause to its source.
fn describe(err: &dyn Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Seconds since 1970 UTC, as the database stores times.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Logs a database error, so a problem with one lurker doesn't drop the
/// connection.
fn log_db<T>(result: rusqlite::Result<T>) -> Option<T> {
    result
        .map_err(|err| eprintln!("error: database: {err}"))
        .ok()
}

/// What lurker knows about its current connection.
struct Session<'a> {
    settings: &'a mut Settings,
    db: &'a Db,
    /// lurker's nick on this connection: the configured one, or an
    /// alternative when that was taken
    nick: String,
    /// Whether the server has accepted the connection
    registered: bool,
    /// The rooms lurker is in
    joined: HashSet<Room>,
    /// Who else is in them
    presence: Presence,
}

impl<'a> Session<'a> {
    fn new(settings: &'a mut Settings, db: &'a Db) -> Session<'a> {
        let nick = settings.config.irc.nick.clone();
        Session {
            settings,
            db,
            nick,
            registered: false,
            joined: HashSet::new(),
            presence: Presence::default(),
        }
    }

    /// Updates what lurker knows from a message, and returns the commands
    /// to send in reply.
    fn handle(&mut self, message: &Message, now: i64) -> Vec<Command> {
        let source = message.source_nickname().unwrap_or("");
        let mut commands = Vec::new();

        match &message.command {
            Command::Response(Response::RPL_WELCOME, args) => {
                if let Some(nick) = args.first() {
                    self.nick = nick.clone();
                }
                self.registered = true;
                eprintln!("Connected as {}", self.nick);
            }

            // Who was already in a room when lurker joined it
            Command::Response(Response::RPL_NAMREPLY, args) => {
                // <me> <symbol> <channel> :<names>
                if let [.., channel, names] = args.as_slice()
                    && let Some(room) = self.settings.config.rooms.find(channel)
                {
                    for name in names.split_whitespace() {
                        let nick = name.trim_start_matches(STATUS_PREFIXES);
                        if !nick.is_empty() && !self.is_own_nick(nick) {
                            self.seen(room, nick, false, now);
                        }
                    }
                }
            }
            Command::Response(Response::RPL_ENDOFNAMES, args) => {
                if let Some(room) = args
                    .get(1)
                    .and_then(|channel| self.settings.config.rooms.find(channel))
                {
                    self.log_room(room);
                }
            }

            Command::JOIN(channels, _, _) => {
                for (room, channel) in self.rooms_in(channels) {
                    if self.is_me(source) {
                        self.joined.insert(room);
                        eprintln!("Joined {channel}");
                    } else {
                        self.seen(room, source, true, now);
                    }
                }
            }
            Command::PART(channels, _) => {
                for (room, channel) in self.rooms_in(channels) {
                    if self.is_me(source) {
                        self.lost_room(room);
                        eprintln!("Left {channel}");
                    } else {
                        self.departed(room, source, "left");
                    }
                }
            }
            Command::KICK(channels, target, reason) => {
                for (room, channel) in self.rooms_in(channels) {
                    if self.is_me(target) {
                        self.lost_room(room);
                        eprintln!(
                            "Kicked from {channel} by {source} ({}), rejoining within a minute",
                            reason.as_deref().unwrap_or("no reason")
                        );
                    } else {
                        self.departed(room, target, "was kicked from");
                    }
                }
            }
            Command::QUIT(_) => {
                if !self.presence.quit(source).is_empty() && self.is_lurker(source) {
                    eprintln!("Lurker {source} quit");
                }
                commands.extend(self.reclaim_nick(source));
            }
            Command::NICK(new_nick) if self.is_me(source) => {
                eprintln!("Now known as {new_nick}");
                self.nick = new_nick.clone();
            }
            Command::NICK(new_nick) => {
                self.presence.rename(source, new_nick);
                match log_db(self.db.rename(source, new_nick)) {
                    Some(NickChange::Followed) => eprintln!("Lurker {source} is now {new_nick}"),
                    Some(NickChange::Taken) => eprintln!(
                        "Lurker {source} is now {new_nick}, which is another lurker's nick, \
                         so lurker still knows them as {source}"
                    ),
                    _ => {}
                }
                commands.extend(self.reclaim_nick(source));
            }

            // Private messages, but not CTCP requests, which the irc crate
            // answers
            Command::PRIVMSG(target, text) if self.is_me(target) && !text.starts_with('\u{1}') => {
                commands.extend(self.private_message(message, text, now));
            }

            Command::ERROR(text) => eprintln!("Server error: {text}"),
            Command::Response(response, args) if (400..600).contains(&(*response as u16)) => {
                eprintln!("warning: {response:?} {}", args.join(" "));
            }
            _ => {}
        }

        commands
    }

    /// JOIN commands for the rooms lurker is not in, once the server has
    /// accepted the connection.
    fn rejoin(&self) -> Vec<Command> {
        if !self.registered {
            return Vec::new();
        }

        Room::ALL
            .into_iter()
            .filter(|room| !self.joined.contains(room))
            .map(|room| {
                Command::JOIN(
                    self.settings.config.rooms.channel(room).to_owned(),
                    None,
                    None,
                )
            })
            .collect()
    }

    /// Reads the settings file again if it changed, and logs what happened.
    fn check_settings(&mut self) {
        match self.settings.reload(false) {
            Some(Ok(summary)) => eprintln!("{summary}"),
            Some(Err(err)) => eprintln!("error: {err}"),
            None => {}
        }
    }

    /// Answers an admin, or ignores anyone else.
    fn private_message(&mut self, message: &Message, text: &str, now: i64) -> Vec<Command> {
        let Some(Prefix::Nickname(nick, _, host)) = &message.prefix else {
            return Vec::new();
        };
        if self.settings.config.admin(nick, host).is_none() {
            eprintln!("Ignored a private message from {nick} ({host}), who isn't an admin");
            return Vec::new();
        }

        eprintln!("{nick}: {text}");
        let command = text.split_whitespace().next().unwrap_or("");
        let lines = if command.eq_ignore_ascii_case("reload") {
            match self.settings.reload(true) {
                Some(Ok(summary)) => vec![summary],
                Some(Err(err)) => vec![err],
                None => Vec::new(),
            }
        } else {
            let context = admin::Context {
                db: self.db,
                rooms: &self.settings.config.rooms,
                presence: &self.presence,
                admin: nick,
                now,
            };
            admin::run(&context, text)
        };

        lines
            .into_iter()
            .map(|line| Command::PRIVMSG(nick.clone(), line))
            .collect()
    }

    /// Someone is in a room: they joined it, or the list of names had them.
    /// A lurker in their own room starts their time there the first time.
    fn seen(&mut self, room: Room, nick: &str, joined: bool, now: i64) {
        self.presence.add(room, nick);

        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return;
        };
        let channel = self.settings.config.rooms.channel(room);
        if lurker.room != room {
            if joined {
                let theirs = self.settings.config.rooms.channel(lurker.room);
                eprintln!("Lurker {nick}, marked for {theirs}, joined {channel}");
            }
            return;
        }

        if log_db(self.db.arrived(lurker.id, now)) == Some(true) {
            eprintln!(
                "Lurker {nick} is in {channel} for the first time; their time there starts now"
            );
        } else if joined {
            eprintln!("Lurker {nick} joined {channel}");
        }
    }

    fn departed(&mut self, room: Room, nick: &str, how: &str) {
        if self.presence.remove(room, nick) && self.is_lurker(nick) {
            let channel = self.settings.config.rooms.channel(room);
            eprintln!("Lurker {nick} {how} {channel}");
        }
    }

    fn lost_room(&mut self, room: Room) {
        self.joined.remove(&room);
        self.presence.clear(room);
    }

    /// Logs how many people and lurkers are in a room.
    fn log_room(&self, room: Room) {
        let lurkers = log_db(self.db.lurkers()).map_or(0, |lurkers| {
            lurkers
                .iter()
                .filter(|lurker| lurker.room == room && self.presence.contains(room, &lurker.nick))
                .count()
        });
        eprintln!(
            "{}: {} people besides lurker, {lurkers} of them lurkers marked for it",
            self.settings.config.rooms.channel(room),
            self.presence.count(room)
        );
    }

    /// Asks for lurker's nick back when whoever had it quit or changed nick.
    fn reclaim_nick(&self, source: &str) -> Option<Command> {
        let nick = &self.settings.config.irc.nick;
        if !casemap::eq(source, nick) || !self.registered || casemap::eq(&self.nick, nick) {
            return None;
        }
        eprintln!("Taking back the nick {nick}");
        Some(Command::NICK(nick.clone()))
    }

    /// The channels in a comma-separated list that are lurker's rooms.
    fn rooms_in<'m>(&self, channels: &'m str) -> Vec<(Room, &'m str)> {
        channels
            .split(',')
            .filter_map(|channel| {
                self.settings
                    .config
                    .rooms
                    .find(channel)
                    .map(|room| (room, channel))
            })
            .collect()
    }

    fn is_lurker(&self, nick: &str) -> bool {
        log_db(self.db.lurker(nick)).flatten().is_some()
    }

    fn is_me(&self, nick: &str) -> bool {
        casemap::eq(nick, &self.nick)
    }

    /// Whether a nick is one lurker itself uses, such as an old connection
    /// of its own that the server hasn't dropped yet.
    fn is_own_nick(&self, nick: &str) -> bool {
        let own = &self.settings.config.irc.nick;
        self.is_me(nick)
            || casemap::eq(nick, own)
            || alt_nicks(own).iter().any(|alt| casemap::eq(nick, alt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(line: &str) -> Message {
        line.parse().unwrap()
    }

    fn join(room: &str) -> Command {
        Command::JOIN(room.to_owned(), None, None)
    }

    fn reply(text: &str) -> Command {
        Command::PRIVMSG("rachel".to_owned(), text.to_owned())
    }

    /// Feeds raw lines to a session, returning what it sends back.
    fn feed(session: &mut Session<'_>, lines: &[&str]) -> Vec<Command> {
        lines
            .iter()
            .flat_map(|line| session.handle(&message(line), 1000))
            .collect()
    }

    /// A session that has connected as `nick` and joined both rooms.
    fn in_both_rooms<'a>(settings: &'a mut Settings, db: &'a Db, nick: &str) -> Session<'a> {
        let mut session = Session::new(settings, db);
        let lines = [
            format!(":irc.example.net 001 {nick} :Welcome"),
            format!(":{nick}!lurker@host JOIN #purgatory"),
            format!(":{nick}!lurker@host JOIN :#FBI_watchlist"),
        ];
        feed(&mut session, &lines.each_ref().map(String::as_str));
        session
    }

    fn here_since(db: &Db, nick: &str) -> Option<i64> {
        db.lurker(nick).unwrap().unwrap().here_since
    }

    #[test]
    fn waits_for_the_welcome_before_rejoining() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = Session::new(&mut settings, &db);
        assert!(session.rejoin().is_empty());

        feed(&mut session, &[":irc.example.net 001 lurker :Welcome"]);
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist")]
        );
    }

    #[test]
    fn reads_the_names_in_a_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker_");

        feed(
            &mut session,
            &[
                ":irc.example.net 353 lurker_ = #purgatory :lurker_ @op +Alice bob lurker",
                ":irc.example.net 366 lurker_ #purgatory :End of /NAMES list.",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 3);
        assert!(session.presence.contains(Room::Purgatory, "alice"));
        assert_eq!(here_since(&db, "alice"), Some(1000));
        assert_eq!(db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn starts_a_lurkers_time_when_they_join_their_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        feed(&mut session, &[":alice!~a@h JOIN #FBI_watchlist"]);
        assert_eq!(here_since(&db, "alice"), None);

        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);
        assert_eq!(here_since(&db, "alice"), Some(1000));
    }

    #[test]
    fn follows_people_leaving() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[
                ":alice!~a@h JOIN #purgatory",
                ":alice!~a@h JOIN #FBI_watchlist",
                ":bob!~b@h JOIN #purgatory",
                ":carol!~c@h JOIN #wetfish",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 2);

        feed(
            &mut session,
            &[
                ":bob!~b@h PART #purgatory :bye",
                ":op!op@host KICK #FBI_watchlist alice :out",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 1);
        assert_eq!(session.presence.count(Room::Watchlist), 0);

        feed(&mut session, &[":alice!~a@h QUIT :Ping timeout"]);
        assert_eq!(session.presence.count(Room::Purgatory), 0);
    }

    #[test]
    fn follows_lurkers_nick_changes() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":alice!~a@h NICK alice_"],
        );
        assert!(session.presence.contains(Room::Purgatory, "alice_"));
        assert_eq!(db.lurker("alice_").unwrap().unwrap().nick, "alice_");
        assert_eq!(db.lurker("alice").unwrap(), None);
    }

    #[test]
    fn answers_admins() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);

        let sent = feed(
            &mut session,
            &[":rachel!~r@home.example.net PRIVMSG lurker :trusted alice"],
        );
        assert_eq!(sent, [reply("alice is marked trusted (#purgatory).")]);
        assert_eq!(here_since(&db, "alice"), Some(1000));
        assert_eq!(db.lurker("alice").unwrap().unwrap().marked_by, "rachel");
    }

    #[test]
    fn reloads_the_settings_for_admins() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(
            &mut session,
            &[":rachel!~r@home.example.net PRIVMSG lurker :reload"],
        );
        let [Command::PRIVMSG(to, text)] = sent.as_slice() else {
            panic!("expected one reply, got {sent:?}");
        };
        assert_eq!(to, "rachel");
        assert!(text.ends_with("again: 1 admin"), "{text}");
    }

    #[test]
    fn ignores_everyone_else() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(
            &mut session,
            &[
                ":rachel!~r@elsewhere.net PRIVMSG lurker :trusted alice",
                ":mallory!~m@home.example.net PRIVMSG lurker :trusted alice",
                ":rachel!~r@home.example.net PRIVMSG #purgatory :trusted alice",
                ":rachel!~r@home.example.net PRIVMSG lurker :\u{1}VERSION\u{1}",
            ],
        );
        assert!(sent.is_empty());
        assert_eq!(db.lurker("alice").unwrap(), None);
    }

    #[test]
    fn rejoins_after_a_kick_or_part() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);

        feed(&mut session, &[":op!op@host KICK #purgatory lurker :out"]);
        assert_eq!(session.presence.count(Room::Purgatory), 0);
        assert_eq!(session.rejoin(), [join("#purgatory")]);

        feed(&mut session, &[":lurker!lurker@host PART #FBI_watchlist"]);
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist")]
        );
    }

    #[test]
    fn follows_its_own_nick_changes() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker_");

        let sent = feed(&mut session, &[":lurker_!lurker@host NICK lurker"]);
        assert!(sent.is_empty());
        assert_eq!(session.nick, "lurker");

        feed(&mut session, &[":op!op@host KICK #purgatory lurker :out"]);
        assert_eq!(session.rejoin(), [join("#purgatory")]);
    }

    #[test]
    fn takes_its_nick_back_when_it_is_freed() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker_");

        let sent = feed(
            &mut session,
            &[":lurker!lurker@old.host QUIT :Ping timeout"],
        );
        assert_eq!(sent, [Command::NICK("lurker".to_owned())]);

        let sent = feed(&mut session, &[":lurker!someone@host NICK someone"]);
        assert_eq!(sent, [Command::NICK("lurker".to_owned())]);
    }

    #[test]
    fn keeps_its_nick_when_it_already_has_it() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(&mut session, &[":alice!alice@host QUIT :bye"]);
        assert!(sent.is_empty());
    }
}
