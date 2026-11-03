//! Keeps lurker on IRC: connects over TLS, joins both rooms and rejoins them
//! when needed, takes its nick back when it was taken, reconnects after a
//! lost connection, and quits with a message when lurker is stopped. Along
//! the way it follows who is in the rooms, answers admins, and talks to the
//! lurkers: a welcome when they first join their room, fanfare when they
//! speak, and a ping about once a week.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use irc::client::ClientStream;
use irc::client::prelude::{Client, Command, Config as IrcConfig, Message, Prefix, Response};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::admin;
use crate::casemap;
use crate::config::{Room, Settings};
use crate::db::{Db, Lurker, NickChange};
use crate::messages::{self, Kind, Messages};
use crate::presence::Presence;
use crate::rng::Rng;
use crate::schedule;
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

/// How often to check whether a lurker's ping is due.
const PING_INTERVAL: Duration = Duration::from_secs(60);

/// After making a fuss about a lurker speaking, lurker stays quiet about
/// them for this many seconds, so a lurker who keeps talking isn't drowned
/// in fanfare and admins aren't flooded with notices.
const FANFARE_COOLDOWN: i64 = 24 * 3600;

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
pub async fn run(settings: &mut Settings, db: &Db, messages: &Messages, shutdown: &mut Shutdown) {
    let mut delay = FIRST_RETRY_DELAY;

    loop {
        let started = Instant::now();

        match connect(settings, db, messages, shutdown).await {
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
async fn connect(
    settings: &mut Settings,
    db: &Db,
    messages: &Messages,
    shutdown: &mut Shutdown,
) -> Result<(), String> {
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
    let mut session = Session::new(settings, db, messages);
    serve(&client, &mut stream, &mut session, shutdown).await
}

/// Handles messages, rejoins rooms, rereads the settings and sends pings
/// until the connection is lost or lurker is stopped.
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
    let mut pings = time::interval(PING_INTERVAL);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
            _ = pings.tick() => session.ping_due(unix_now()),
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
    messages: &'a Messages,
    rng: Rng,
    /// lurker's nick on this connection: the configured one, or an
    /// alternative when that was taken
    nick: String,
    /// Whether the server has accepted the connection
    registered: bool,
    /// The rooms lurker is in
    joined: HashSet<Room>,
    /// Who else is in them
    presence: Presence,
    /// When lurker last pinged someone in each room, to space pings out
    last_ping: HashMap<Room, i64>,
}

impl<'a> Session<'a> {
    fn new(settings: &'a mut Settings, db: &'a Db, messages: &'a Messages) -> Session<'a> {
        let nick = settings.config.irc.nick.clone();
        Session {
            settings,
            db,
            messages,
            rng: Rng::from_clock(),
            nick,
            registered: false,
            joined: HashSet::new(),
            presence: Presence::default(),
            last_ping: HashMap::new(),
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
                            // Being listed is not joining, so nobody is welcomed
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
                        commands.extend(self.seen(room, source, true, now));
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
            // Someone speaking in one of the rooms, including /me
            Command::PRIVMSG(target, text)
                if !text.starts_with('\u{1}') || text.starts_with("\u{1}ACTION") =>
            {
                if let Some(room) = self.settings.config.rooms.find(target) {
                    commands.extend(self.spoke(room, source, text, now));
                }
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
        let mut words = text.split_whitespace();
        let command = words.next().unwrap_or("");
        let mut commands = Vec::new();
        let lines = if command.eq_ignore_ascii_case("reload") {
            match self.settings.reload(true) {
                Some(Ok(summary)) => vec![summary],
                Some(Err(err)) => vec![err],
                None => Vec::new(),
            }
        } else if command.eq_ignore_ascii_case("poke") {
            let (reply, ping) = self.poke(words.next(), words.next().is_some(), now);
            commands.extend(ping);
            vec![reply]
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

        commands.extend(
            lines
                .into_iter()
                .map(|line| Command::PRIVMSG(nick.clone(), line)),
        );
        commands
    }

    /// An admin's `poke <nick>`: the lurker's ping, sent now. Returns the
    /// reply to the admin, and the ping if one is sent.
    fn poke(&mut self, nick: Option<&str>, extra: bool, now: i64) -> (String, Option<Command>) {
        let (Some(nick), false) = (nick, extra) else {
            return ("Usage: poke <nick>".to_owned(), None);
        };
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return (format!("{nick} isn't marked."), None);
        };
        let channel = self.settings.config.rooms.channel(lurker.room).to_owned();
        if !self.joined.contains(&lurker.room) || !self.presence.contains(lurker.room, &lurker.nick)
        {
            return (
                format!("{} isn't in {channel} right now.", lurker.nick),
                None,
            );
        }

        match self.ping(&lurker, now) {
            Some(ping) => (format!("Poked {} in {channel}.", lurker.nick), Some(ping)),
            None => (
                format!("lurker has nothing to say to {}.", lurker.nick),
                None,
            ),
        }
    }

    /// Pings lurkers whose weekly ping is due and who are in their room, at
    /// most one per room every `schedule::ROOM_GAP` seconds. Lurkers who
    /// have started their time in their room but have no ping yet get one
    /// scheduled.
    fn ping_due(&mut self, now: i64) -> Vec<Command> {
        let Some(lurkers) = log_db(self.db.lurkers()) else {
            return Vec::new();
        };

        for lurker in &lurkers {
            if let (Some(here_since), None) = (lurker.here_since, lurker.next_ping_at) {
                let at = schedule::first_ping(here_since, now, &mut self.rng);
                log_db(self.db.set_next_ping(lurker.id, at));
            }
        }

        let mut commands = Vec::new();
        for room in Room::ALL {
            let spaced = self
                .last_ping
                .get(&room)
                .is_none_or(|&last| now - last >= schedule::ROOM_GAP);
            if !self.joined.contains(&room) || !spaced {
                continue;
            }

            let due = lurkers
                .iter()
                .filter(|lurker| {
                    lurker.room == room
                        && lurker.next_ping_at.is_some_and(|at| at <= now)
                        && self.presence.contains(room, &lurker.nick)
                })
                .min_by_key(|lurker| lurker.next_ping_at);
            if let Some(lurker) = due {
                commands.extend(self.ping(lurker, now));
            }
        }
        commands
    }

    /// A lurker's ping, a message they haven't heard yet if there is one,
    /// and schedules their next one.
    fn ping(&mut self, lurker: &Lurker, now: i64) -> Option<Command> {
        let used = log_db(self.db.used_pings(lurker.id)).unwrap_or_default();
        let (template, text) =
            self.messages
                .choose(lurker.room, Kind::Ping, lurker, now, &mut self.rng, &used)?;

        // Once they have heard every ping they can, a new round starts
        if used.contains(&template) {
            log_db(self.db.clear_used_pings(lurker.id));
        }
        log_db(self.db.use_ping(lurker.id, template));
        log_db(
            self.db
                .set_next_ping(lurker.id, schedule::next_ping(now, &mut self.rng)),
        );
        self.last_ping.insert(lurker.room, now);

        let channel = self.settings.config.rooms.channel(lurker.room);
        eprintln!("Pinged {} in {channel}", lurker.nick);
        Some(Command::PRIVMSG(channel.to_owned(), text))
    }

    /// Someone spoke in a room. When it is a lurker in their own room, and
    /// lurker hasn't made a fuss about them in the last day, it plays the
    /// fanfare and tells the admins what they said.
    fn spoke(&mut self, room: Room, nick: &str, text: &str, now: i64) -> Vec<Command> {
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return Vec::new();
        };
        if lurker.room != room
            || lurker
                .fanfare_at
                .is_some_and(|at| now - at < FANFARE_COOLDOWN)
        {
            return Vec::new();
        }
        log_db(self.db.set_fanfare(lurker.id, now));

        let channel = self.settings.config.rooms.channel(room).to_owned();
        let mut commands = Vec::new();
        if let Some((_, fanfare)) =
            self.messages
                .choose(room, Kind::Fanfare, &lurker, now, &mut self.rng, &[])
        {
            commands.push(Command::PRIVMSG(channel.clone(), fanfare));
        }

        // Admins who aren't online miss the notice; the server says so
        let notice = messages::notice(&channel, &lurker, text, now);
        let admins = &self.settings.config.admins;
        for admin in admins {
            commands.push(Command::PRIVMSG(admin.nick.clone(), notice.clone()));
        }
        eprintln!(
            "Lurker {} spoke in {channel}; told {} admins",
            lurker.nick,
            admins.len()
        );
        commands
    }

    /// Someone is in a room: they joined it, or the list of names had them.
    /// A lurker in their own room starts their time there the first time,
    /// and is welcomed if that is because they joined it. Returns the
    /// welcome, if any.
    fn seen(&mut self, room: Room, nick: &str, joined: bool, now: i64) -> Option<Command> {
        self.presence.add(room, nick);

        let lurker = log_db(self.db.lurker(nick)).flatten()?;
        let channel = self.settings.config.rooms.channel(room).to_owned();
        if lurker.room != room {
            if joined {
                let theirs = self.settings.config.rooms.channel(lurker.room);
                eprintln!("Lurker {nick}, marked for {theirs}, joined {channel}");
            }
            return None;
        }

        if log_db(self.db.arrived(lurker.id, now)) != Some(true) {
            if joined {
                eprintln!("Lurker {nick} joined {channel}");
            }
            return None;
        }
        eprintln!("Lurker {nick} is in {channel} for the first time; their time there starts now");
        if !joined {
            return None;
        }

        let lurker = Lurker {
            here_since: Some(now),
            ..lurker
        };
        let (_, welcome) =
            self.messages
                .choose(room, Kind::Welcome, &lurker, now, &mut self.rng, &[])?;
        eprintln!("Welcomed {nick} to {channel}");
        Some(Command::PRIVMSG(channel, welcome))
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
    use std::sync::OnceLock;

    use super::*;

    const DAY: i64 = 86_400;

    fn messages() -> &'static Messages {
        static MESSAGES: OnceLock<Messages> = OnceLock::new();
        MESSAGES.get_or_init(|| Messages::builtin().unwrap())
    }

    /// The channel and text of a message lurker sends.
    fn said(command: &Command) -> (&str, &str) {
        match command {
            Command::PRIVMSG(target, text) => (target.as_str(), text.as_str()),
            other => panic!("expected a PRIVMSG, got {other:?}"),
        }
    }

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
        let mut session = Session::new(settings, db, messages());
        session.rng = Rng::seeded(1);
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
        let mut session = Session::new(&mut settings, &db, messages());
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
    fn welcomes_lurkers_the_first_time_they_join() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Watchlist, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        let sent = feed(&mut session, &[":alice!~a@h JOIN #FBI_watchlist"]);
        let [welcome] = sent.as_slice() else {
            panic!("expected a welcome, got {sent:?}");
        };
        let (channel, text) = said(welcome);
        assert_eq!(channel, "#FBI_watchlist");
        assert!(text.contains("alice"), "{text}");

        let sent = feed(
            &mut session,
            &[
                ":alice!~a@h PART #FBI_watchlist",
                ":alice!~a@h JOIN #FBI_watchlist",
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[test]
    fn does_not_welcome_lurkers_already_in_the_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(
            &mut session,
            &[":irc.example.net 353 lurker = #purgatory :lurker alice"],
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(here_since(&db, "alice"), Some(1000));
    }

    #[test]
    fn makes_a_fuss_when_a_lurker_speaks_once_a_day() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        db.mark("bob", Room::Watchlist, false, "rachel", 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":bob!~b@h JOIN #purgatory"],
        );

        let sent = feed(&mut session, &[":alice!~a@h PRIVMSG #purgatory :hello?"]);
        let [fanfare, notice] = sent.as_slice() else {
            panic!("expected fanfare and a notice, got {sent:?}");
        };
        assert_eq!(said(fanfare).0, "#purgatory");
        assert_eq!(
            said(notice),
            (
                "rachel",
                "#purgatory: alice spoke after 0 days in the room (no notes recorded): \"hello?\""
            )
        );

        // Not again the same day, not for someone who isn't a lurker, and
        // not for a lurker outside their own room
        let sent = feed(
            &mut session,
            &[
                ":alice!~a@h PRIVMSG #purgatory :anyone?",
                ":carol!~c@h PRIVMSG #purgatory :hi",
                ":bob!~b@h PRIVMSG #purgatory :hi",
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");

        let later = session.handle(
            &message(":alice!~a@h PRIVMSG #purgatory :\u{1}ACTION is back\u{1}"),
            1000 + DAY,
        );
        assert_eq!(later.len(), 2);
        assert!(said(&later[1]).1.ends_with("\"/me is back\""));
    }

    #[test]
    fn pings_lurkers_when_their_ping_is_due() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        db.mark("bob", Room::Purgatory, false, "rachel", 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":bob!~b@h JOIN #purgatory"],
        );

        // Their first pings are scheduled 3 to 7 days after joining
        assert!(session.ping_due(2000).is_empty());
        let first = db.lurker("alice").unwrap().unwrap().next_ping_at.unwrap();
        assert!((1000 + 3 * DAY..1000 + 7 * DAY).contains(&first));

        let alice = db.lurker("alice").unwrap().unwrap().id;
        let bob = db.lurker("bob").unwrap().unwrap().id;
        db.set_next_ping(alice, 1500).unwrap();
        db.set_next_ping(bob, 1600).unwrap();

        let sent = session.ping_due(2000);
        let [ping] = sent.as_slice() else {
            panic!("expected one ping, got {sent:?}");
        };
        let (channel, text) = said(ping);
        assert_eq!(channel, "#purgatory");
        assert!(text.starts_with("alice"), "{text}");
        let next = db.lurker("alice").unwrap().unwrap().next_ping_at.unwrap();
        assert!(next >= 2000 + 7 * DAY);
        assert_eq!(db.used_pings(alice).unwrap().len(), 1);

        // Bob waits until the room has been quiet for a while
        assert!(session.ping_due(2000 + schedule::ROOM_GAP - 1).is_empty());
        let sent = session.ping_due(2000 + schedule::ROOM_GAP);
        assert!(said(&sent[0]).1.starts_with("bob"), "{sent:?}");
    }

    #[test]
    fn waits_for_lurkers_to_be_in_the_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":alice!~a@h QUIT :bye"],
        );
        let alice = db.lurker("alice").unwrap().unwrap().id;
        db.set_next_ping(alice, 1500).unwrap();

        assert!(session.ping_due(2000).is_empty());
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);
        assert_eq!(session.ping_due(3000).len(), 1);
    }

    #[test]
    fn admins_can_poke_lurkers() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.mark("alice", Room::Purgatory, false, "rachel", 0)
            .unwrap();
        db.mark("bob", Room::Purgatory, false, "rachel", 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);

        let sent = feed(
            &mut session,
            &[":rachel!~r@home.example.net PRIVMSG lurker :poke alice"],
        );
        let [ping, reply] = sent.as_slice() else {
            panic!("expected a ping and a reply, got {sent:?}");
        };
        assert_eq!(said(ping).0, "#purgatory");
        assert_eq!(said(reply), ("rachel", "Poked alice in #purgatory."));

        let sent = feed(
            &mut session,
            &[
                ":rachel!~r@home.example.net PRIVMSG lurker :poke bob",
                ":rachel!~r@home.example.net PRIVMSG lurker :poke carol",
                ":rachel!~r@home.example.net PRIVMSG lurker :poke",
            ],
        );
        let replies: Vec<&str> = sent.iter().map(|command| said(command).1).collect();
        assert_eq!(
            replies,
            [
                "bob isn't in #purgatory right now.",
                "carol isn't marked.",
                "Usage: poke <nick>"
            ]
        );
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
