//! Keeps lurker on IRC: connects over TLS, joins its rooms and the channels
//! admins asked it to join, rejoins them when needed, takes its nick back
//! when it was taken, reconnects after a lost connection, and quits with a
//! message when lurker is stopped. Along the way it follows who is in its
//! channels, answers admins, and deals with lurkers: a welcome when they
//! first join their room, fanfare when they speak, a ping about once a week,
//! and in channels where lurker has +h or +o, a warning and then a kick.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use irc::client::ClientStream;
use irc::client::prelude::{
    Capability, ChannelMode, Client, Command, Config as IrcConfig, Message, Mode, Prefix, Response,
};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::admin;
use crate::casemap;
use crate::config::{Room, Settings};
use crate::db::{Db, Lurker, NickChange};
use crate::messages::{self, Facts, Kind, Messages};
use crate::presence::Presence;
use crate::rng::Rng;
use crate::schedule;
use crate::shutdown::Shutdown;

/// Sent with QUIT when lurker is stopped, which is usually for a deploy.
pub const QUIT_MESSAGE: &str = "brb, getting a software update";

/// Sent with PART when an admin asks lurker to leave a channel.
const PART_MESSAGE: &str = "An admin asked me to leave";

/// How long connecting, including the TLS handshake, may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the server to close the connection after QUIT.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to join the channels lurker is not in, such as after a kick.
const REJOIN_INTERVAL: Duration = Duration::from_secs(60);

/// How often to check whether the settings file changed.
const SETTINGS_INTERVAL: Duration = Duration::from_secs(30);

/// How often to check whether a lurker's ping, warning or kick is due.
const LURKER_INTERVAL: Duration = Duration::from_secs(60);

/// After making a fuss about a lurker speaking, lurker stays quiet about
/// them for this many seconds, so a lurker who keeps talking isn't drowned
/// in fanfare and admins aren't flooded with notices.
const FANFARE_COOLDOWN: i64 = 24 * 3600;

/// How many seconds a kicked lurker has to join their room after being
/// invited, before admins are asked to sajoin them.
const INVITE_WAIT: i64 = 120;

/// The wait before reconnecting, doubled after each attempt up to
/// `MAX_RETRY_DELAY`.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// A connection that lasted this long counts as having worked, so the wait
/// before reconnecting starts again from `FIRST_RETRY_DELAY`.
const STABLE_CONNECTION: Duration = Duration::from_secs(300);

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

    let irc = irc_config(settings, db);
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

    // multi-prefix lists every status a nick has in the names list, so
    // lurker knows it can still kick after losing +o while keeping +h
    client
        .send_cap_req(&[Capability::MultiPrefix])
        .map_err(|err| describe(&err))?;
    client.identify().map_err(|err| describe(&err))?;
    let mut stream = client.stream().map_err(|err| describe(&err))?;
    let mut session = Session::new(settings, db, messages);
    serve(&client, &mut stream, &mut session, shutdown).await
}

/// Handles messages, rejoins channels, rereads the settings and deals with
/// lurkers until the connection is lost or lurker is stopped.
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
    let mut lurkers = time::interval(LURKER_INTERVAL);
    lurkers.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
            _ = lurkers.tick() => {
                let now = unix_now();
                let mut commands = session.ping_due(now);
                commands.extend(session.enforce(now));
                commands
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

/// The irc crate's settings for one connection. The crate joins lurker's
/// rooms and channels once the server has accepted the connection, answers
/// server pings, and tries the alternative nicks when the nick is taken.
fn irc_config(settings: &Settings, db: &Db) -> IrcConfig {
    let config = &settings.config;
    let nick = &config.irc.nick;
    let mut channels: Vec<String> = config.rooms.all().map(str::to_owned).to_vec();
    channels.extend(log_db(db.channels()).unwrap_or_default());

    IrcConfig {
        nickname: Some(nick.clone()),
        alt_nicks: alt_nicks(nick).to_vec(),
        username: Some(config.irc.username().to_owned()),
        realname: Some(config.irc.realname().to_owned()),
        server: Some(config.irc.server.clone()),
        port: Some(config.irc.port),
        use_tls: Some(true),
        channels,
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

/// The channel status a prefix in the names list stands for, such as `o`
/// for `@`.
fn status_of_prefix(prefix: char) -> Option<char> {
    match prefix {
        '~' => Some('q'),
        '&' => Some('a'),
        '@' => Some('o'),
        '%' => Some('h'),
        '+' => Some('v'),
        _ => None,
    }
}

/// The channel status a mode change is about, such as `o` for +o.
fn status_of_mode(mode: &ChannelMode) -> Option<char> {
    match mode {
        ChannelMode::Founder => Some('q'),
        ChannelMode::Admin => Some('a'),
        ChannelMode::Oper => Some('o'),
        ChannelMode::Halfop => Some('h'),
        _ => None,
    }
}

/// Whether a channel status lets lurker kick: +h or higher.
fn can_kick_with(status: char) -> bool {
    "qaoh".contains(status)
}

/// A lurker kicked from lurker's channels and invited to their room, who
/// admins are asked to sajoin if they don't join it.
struct Invited {
    at: i64,
    from: Vec<String>,
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
    /// The channels lurker is in, folded
    joined: HashSet<String>,
    /// lurker's own status in each channel, such as `o` for +o, by folded
    /// channel name
    status: HashMap<String, HashSet<char>>,
    /// Who else is in them
    presence: Presence,
    /// When lurker last pinged someone in each room, to space pings out
    last_ping: HashMap<Room, i64>,
    /// Lurkers kicked and invited to their room, by lurker id
    invited: HashMap<i64, Invited>,
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
            status: HashMap::new(),
            presence: Presence::default(),
            last_ping: HashMap::new(),
            invited: HashMap::new(),
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

            // Who was already in a channel when lurker joined it
            Command::Response(Response::RPL_NAMREPLY, args) => {
                // <me> <symbol> <channel> :<names>
                if let [.., channel, names] = args.as_slice() {
                    for name in names.split_whitespace() {
                        self.listed(channel, name, now);
                    }
                }
            }
            Command::Response(Response::RPL_ENDOFNAMES, args) => {
                if let Some(channel) = args.get(1) {
                    self.log_channel(channel);
                    if self.room_of(channel).is_none() {
                        commands.extend(self.enforce(now));
                    }
                }
            }

            Command::JOIN(channels, _, _) => {
                for channel in channels.split(',') {
                    if self.is_me(source) {
                        self.joined.insert(casemap::fold(channel));
                        eprintln!("Joined {channel}");
                        continue;
                    }
                    self.presence.add(channel, source);
                    match self.room_of(channel) {
                        Some(room) => commands.extend(self.seen(room, source, true, now)),
                        None => commands.extend(self.enforce(now)),
                    }
                }
            }
            Command::PART(channels, _) => {
                for channel in channels.split(',') {
                    if self.is_me(source) {
                        self.lost_channel(channel);
                        eprintln!("Left {channel}");
                    } else {
                        self.departed(channel, source, "left");
                    }
                }
            }
            Command::KICK(channels, target, reason) => {
                for channel in channels.split(',') {
                    if self.is_me(target) {
                        self.lost_channel(channel);
                        eprintln!(
                            "Kicked from {channel} by {source} ({}), rejoining within a minute",
                            reason.as_deref().unwrap_or("no reason")
                        );
                    } else {
                        self.departed(channel, target, "was kicked from");
                    }
                }
            }
            Command::QUIT(_) => {
                if self.presence.quit(source) && self.is_lurker(source) {
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
            Command::ChannelMODE(channel, modes) => {
                if self.mode_changed(channel, modes) {
                    commands.extend(self.enforce(now));
                }
            }

            Command::INVITE(target, channel) if self.is_me(target) => {
                commands.extend(self.invited_to(message, channel, now));
            }

            // Private messages, but not CTCP requests, which the irc crate
            // answers
            Command::PRIVMSG(target, text) if self.is_me(target) && !text.starts_with('\u{1}') => {
                commands.extend(self.private_message(message, text, now));
            }
            // Messages in a channel: an admin addressing lurker, or someone
            // speaking, including with /me
            Command::PRIVMSG(target, text)
                if !text.starts_with('\u{1}') || text.starts_with("\u{1}ACTION") =>
            {
                commands.extend(self.channel_message(message, target, text, now));
            }

            Command::ERROR(text) => eprintln!("Server error: {text}"),
            Command::Response(response, args) if (400..600).contains(&(*response as u16)) => {
                eprintln!("warning: {response:?} {}", args.join(" "));
            }
            _ => {}
        }

        commands
    }

    /// JOIN commands for the rooms and channels lurker is not in, once the
    /// server has accepted the connection.
    fn rejoin(&self) -> Vec<Command> {
        if !self.registered {
            return Vec::new();
        }

        self.wanted()
            .into_iter()
            .filter(|channel| !self.joined.contains(&casemap::fold(channel)))
            .map(|channel| Command::JOIN(channel, None, None))
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

    /// Answers an admin's private message, or ignores anyone else.
    fn private_message(&mut self, message: &Message, text: &str, now: i64) -> Vec<Command> {
        let Some(nick) = self.admin_nick(message) else {
            if let Some(Prefix::Nickname(nick, _, host)) = &message.prefix {
                eprintln!("Ignored a private message from {nick} ({host}), who isn't an admin");
            }
            return Vec::new();
        };

        let (mut commands, lines) = self.command(&nick, text, now);
        commands.extend(
            lines
                .into_iter()
                .map(|line| Command::PRIVMSG(nick.clone(), line)),
        );
        commands
    }

    /// A message in a channel. An admin starting it with lurker's nick gives
    /// a command, answered in the channel. Otherwise it may be a lurker
    /// speaking in their room.
    fn channel_message(
        &mut self,
        message: &Message,
        channel: &str,
        text: &str,
        now: i64,
    ) -> Vec<Command> {
        if let Some(command) = self.addressed(text)
            && let Some(nick) = self.admin_nick(message)
        {
            let (mut commands, lines) = self.command(&nick, command, now);
            commands.extend(
                lines
                    .into_iter()
                    .map(|line| Command::PRIVMSG(channel.to_owned(), format!("{nick}: {line}"))),
            );
            return commands;
        }

        let source = message.source_nickname().unwrap_or("");
        match self.room_of(channel) {
            Some(room) => self.spoke(room, source, text, now),
            None => Vec::new(),
        }
    }

    /// The text after lurker's nick, when a message starts with it, as in
    /// `lurker: list`, `lurker, list` or `lurker list`.
    fn addressed<'t>(&self, text: &'t str) -> Option<&'t str> {
        let length = self.nick.len();
        let start = text.get(..length)?;
        let rest = text.get(length..)?;
        if !casemap::eq(start, &self.nick) {
            return None;
        }
        let rest = rest.strip_prefix([':', ',']).unwrap_or(rest);
        if !rest.starts_with(' ') {
            return None;
        }
        Some(rest.trim())
    }

    /// The nick of the admin a message came from, if it is from an admin.
    fn admin_nick(&self, message: &Message) -> Option<String> {
        let Some(Prefix::Nickname(nick, _, host)) = &message.prefix else {
            return None;
        };
        self.settings.config.admin(nick, host).map(|_| nick.clone())
    }

    /// Carries out an admin's command. Returns the commands it sends, such as
    /// a JOIN, and the lines to reply with.
    fn command(&mut self, admin: &str, text: &str, now: i64) -> (Vec<Command>, Vec<String>) {
        eprintln!("{admin}: {text}");
        let mut words = text.split_whitespace();
        let name = words.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<&str> = words.collect();

        match name.as_str() {
            "reload" => {
                let lines = match self.settings.reload(true) {
                    Some(Ok(summary)) => vec![summary],
                    Some(Err(err)) => vec![err],
                    None => Vec::new(),
                };
                (Vec::new(), lines)
            }
            "poke" => {
                let (reply, ping) = self.poke(&args, now);
                (ping.into_iter().collect(), vec![reply])
            }
            "join" => self.join_channel(&args, admin, now),
            "part" => self.part_channel(&args),
            "channels" => (Vec::new(), vec![self.channels_report()]),
            _ => {
                let context = admin::Context {
                    db: self.db,
                    rooms: &self.settings.config.rooms,
                    presence: &self.presence,
                    admin,
                    now,
                };
                (Vec::new(), admin::run(&context, text))
            }
        }
    }

    /// An invitation to a channel: an admin's is the same as `join`, and
    /// anyone else's is ignored.
    fn invited_to(&mut self, message: &Message, channel: &str, now: i64) -> Vec<Command> {
        let Some(admin) = self.admin_nick(message) else {
            let from = message.source_nickname().unwrap_or("someone");
            eprintln!("Ignored an invitation to {channel} from {from}, who isn't an admin");
            return Vec::new();
        };
        let (mut commands, lines) = self.join_channel(&[channel], &admin, now);
        commands.extend(
            lines
                .into_iter()
                .map(|line| Command::PRIVMSG(admin.clone(), line)),
        );
        commands
    }

    /// `join <#channel>`: adds a channel for lurker to join and joins it.
    fn join_channel(
        &mut self,
        args: &[&str],
        admin: &str,
        now: i64,
    ) -> (Vec<Command>, Vec<String>) {
        let reply = |text: String| (Vec::new(), vec![text]);
        let [channel] = args else {
            return reply("Usage: join <#channel>".to_owned());
        };
        if !channel.starts_with('#') || channel.len() < 2 || channel.contains(',') {
            return reply(format!("{channel} isn't a channel. Channels start with #."));
        }
        if self.room_of(channel).is_some() {
            return reply(format!(
                "{channel} is one of my rooms, so I'm always in it."
            ));
        }

        let Some(added) = log_db(self.db.add_channel(channel, admin, now)) else {
            return reply("Something went wrong with the database, so I didn't join.".to_owned());
        };
        if !added && self.joined.contains(&casemap::fold(channel)) {
            return reply(format!("I'm already in {channel}."));
        }
        eprintln!("Joining {channel}, as {admin} asked");
        (
            vec![Command::JOIN((*channel).to_owned(), None, None)],
            vec![format!(
                "Joining {channel}. Give me +h or +o there so I can warn and kick lurkers."
            )],
        )
    }

    /// `part <#channel>`: leaves a channel and stops rejoining it.
    fn part_channel(&mut self, args: &[&str]) -> (Vec<Command>, Vec<String>) {
        let reply = |text: String| (Vec::new(), vec![text]);
        let [channel] = args else {
            return reply("Usage: part <#channel>".to_owned());
        };
        if self.room_of(channel).is_some() {
            return reply(format!("{channel} is one of my rooms, so I stay in it."));
        }
        match log_db(self.db.drop_channel(channel)) {
            Some(true) => (
                vec![Command::PART(
                    (*channel).to_owned(),
                    Some(PART_MESSAGE.to_owned()),
                )],
                vec![format!("Leaving {channel}.")],
            ),
            Some(false) => reply(format!("{channel} isn't one of my channels.")),
            None => reply("Something went wrong with the database, so I didn't leave.".to_owned()),
        }
    }

    /// `channels`: the channels admins asked lurker to join, and whether it
    /// can kick in each.
    fn channels_report(&self) -> String {
        let channels = log_db(self.db.channels()).unwrap_or_default();
        if channels.is_empty() {
            return "I'm only in my rooms. Invite me to a channel, or send: join <#channel>"
                .to_owned();
        }
        let described: Vec<String> = channels
            .iter()
            .map(|channel| {
                let state = if !self.joined.contains(&casemap::fold(channel)) {
                    "not in it right now"
                } else if self.can_kick(channel) {
                    "can kick"
                } else {
                    "needs +h or +o"
                };
                format!("{channel} ({state})")
            })
            .collect();
        format!("My channels: {}", described.join(", "))
    }

    /// An admin's `poke <nick>`: the lurker's ping, sent now. Returns the
    /// reply to the admin, and the ping if one is sent.
    fn poke(&mut self, args: &[&str], now: i64) -> (String, Option<Command>) {
        let [nick] = args else {
            return ("Usage: poke <nick>".to_owned(), None);
        };
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return (format!("{nick} isn't marked."), None);
        };
        let channel = self.room_channel(lurker.room).to_owned();
        if !self.joined.contains(&casemap::fold(&channel))
            || !self.presence.contains(&channel, &lurker.nick)
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
            let channel = self.room_channel(room).to_owned();
            let spaced = self
                .last_ping
                .get(&room)
                .is_none_or(|&last| now - last >= schedule::ROOM_GAP);
            if !self.joined.contains(&casemap::fold(&channel)) || !spaced {
                continue;
            }

            let due = lurkers
                .iter()
                .filter(|lurker| {
                    lurker.room == room
                        && lurker.next_ping_at.is_some_and(|at| at <= now)
                        && self.presence.contains(&channel, &lurker.nick)
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
        let channel = self.room_channel(lurker.room).to_owned();
        let facts = Facts {
            lurker,
            room: &channel,
            now,
        };
        let (template, text) = self
            .messages
            .choose(Kind::Ping, &facts, &mut self.rng, &used)?;

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

        eprintln!("Pinged {} in {channel}", lurker.nick);
        Some(Command::PRIVMSG(channel, text))
    }

    /// Deals with lurkers who have a grace period and are in a channel where
    /// lurker can kick: warns them once, and kicks them once their grace
    /// period after the warning is over, inviting them to their room. Also
    /// asks admins to sajoin lurkers who were invited a while ago and
    /// haven't joined.
    fn enforce(&mut self, now: i64) -> Vec<Command> {
        let Some(lurkers) = log_db(self.db.lurkers()) else {
            return Vec::new();
        };
        let kickable: Vec<String> = log_db(self.db.channels())
            .unwrap_or_default()
            .into_iter()
            .filter(|channel| self.can_kick(channel))
            .collect();

        let mut commands = Vec::new();
        for lurker in &lurkers {
            if lurker.grace.is_none() {
                continue;
            }
            let present: Vec<String> = kickable
                .iter()
                .filter(|channel| self.presence.contains(channel, &lurker.nick))
                .cloned()
                .collect();
            let Some(first) = present.first() else {
                continue;
            };

            match lurker.kick_at() {
                None => commands.extend(self.warn(lurker, first, now)),
                Some(at) if at <= now => commands.extend(self.kick(lurker, &present, now)),
                Some(_) => {}
            }
        }

        commands.extend(self.ask_for_sajoins(&lurkers, now));
        commands
    }

    /// Warns a lurker in a channel that they will be kicked.
    fn warn(&mut self, lurker: &Lurker, channel: &str, now: i64) -> Option<Command> {
        let room = self.room_channel(lurker.room).to_owned();
        let facts = Facts {
            lurker,
            room: &room,
            now,
        };
        let (_, warning) = self
            .messages
            .choose(Kind::Warning, &facts, &mut self.rng, &[])?;
        log_db(self.db.set_warned(lurker.id, now));
        eprintln!(
            "Warned {} in {channel}; kicked in {}",
            lurker.nick,
            crate::dates::length(lurker.grace.unwrap_or_default())
        );
        Some(Command::PRIVMSG(channel.to_owned(), warning))
    }

    /// Kicks a lurker from channels, and invites them to their room if they
    /// aren't in it.
    fn kick(&mut self, lurker: &Lurker, channels: &[String], now: i64) -> Vec<Command> {
        let room = self.room_channel(lurker.room).to_owned();
        let mut commands = Vec::new();

        for channel in channels {
            let facts = Facts {
                lurker,
                room: &room,
                now,
            };
            let reason = self
                .messages
                .choose(Kind::Kick, &facts, &mut self.rng, &[])
                .map(|(_, reason)| reason);
            commands.push(Command::KICK(channel.clone(), lurker.nick.clone(), reason));
            self.presence.remove(channel, &lurker.nick);
        }
        eprintln!("Kicked {} from {}", lurker.nick, channels.join(", "));

        if !self.presence.contains(&room, &lurker.nick) {
            commands.push(Command::INVITE(lurker.nick.clone(), room.clone()));
            self.invited.entry(lurker.id).or_insert_with(|| Invited {
                at: now,
                from: channels.to_vec(),
            });
            eprintln!("Invited {} to {room}", lurker.nick);
        }
        commands
    }

    /// Notices to admins about lurkers invited to their room at least
    /// `INVITE_WAIT` seconds ago who haven't joined it.
    fn ask_for_sajoins(&mut self, lurkers: &[Lurker], now: i64) -> Vec<Command> {
        let mut commands = Vec::new();
        let ids: Vec<i64> = self.invited.keys().copied().collect();

        for id in ids {
            let Some(lurker) = lurkers.iter().find(|lurker| lurker.id == id) else {
                // Forgotten since
                self.invited.remove(&id);
                continue;
            };
            let room = self.room_channel(lurker.room).to_owned();
            if self.presence.contains(&room, &lurker.nick) {
                self.invited.remove(&id);
                continue;
            }
            let Some(invited) = self.invited.get(&id) else {
                continue;
            };
            if now - invited.at < INVITE_WAIT {
                continue;
            }

            let notice = messages::not_joined(lurker, &room, &invited.from);
            for admin in &self.settings.config.admins {
                commands.push(Command::PRIVMSG(admin.nick.clone(), notice.clone()));
            }
            eprintln!(
                "Asked admins to sajoin {}, who hasn't joined {room}",
                lurker.nick
            );
            self.invited.remove(&id);
        }
        commands
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

        let channel = self.room_channel(room).to_owned();
        let facts = Facts {
            lurker: &lurker,
            room: &channel,
            now,
        };
        let mut commands = Vec::new();
        if let Some((_, fanfare)) = self
            .messages
            .choose(Kind::Fanfare, &facts, &mut self.rng, &[])
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

    /// Someone the names list has in a channel. For lurker itself, it gives
    /// lurker's own status there.
    fn listed(&mut self, channel: &str, name: &str, now: i64) {
        let nick = name.trim_start_matches(|c| status_of_prefix(c).is_some());
        if nick.is_empty() {
            return;
        }
        if self.is_own_nick(nick) {
            if self.is_me(nick) {
                let status: HashSet<char> = name.chars().map_while(status_of_prefix).collect();
                self.status.insert(casemap::fold(channel), status);
            }
            return;
        }

        self.presence.add(channel, nick);
        if let Some(room) = self.room_of(channel) {
            // Being listed is not joining, so nobody is welcomed
            self.seen(room, nick, false, now);
        }
    }

    /// Follows a mode change in a channel. Returns true when it let lurker
    /// kick there.
    fn mode_changed(&mut self, channel: &str, modes: &[Mode<ChannelMode>]) -> bool {
        let key = casemap::fold(channel);
        let could_kick = self.can_kick(channel);

        for mode in modes {
            let (adding, mode, target) = match mode {
                Mode::Plus(mode, Some(target)) => (true, mode, target),
                Mode::Minus(mode, Some(target)) => (false, mode, target),
                _ => continue,
            };
            let Some(status) = status_of_mode(mode) else {
                continue;
            };
            if !self.is_me(target) {
                continue;
            }
            let own = self.status.entry(key.clone()).or_default();
            if adding {
                own.insert(status);
            } else {
                own.remove(&status);
            }
            eprintln!(
                "{}{status} for lurker in {channel}",
                if adding { '+' } else { '-' }
            );
        }

        let can_kick = self.can_kick(channel);
        if can_kick != could_kick && self.room_of(channel).is_none() {
            if can_kick {
                eprintln!("lurker can now warn and kick lurkers in {channel}");
            } else {
                eprintln!("lurker can no longer kick in {channel}");
            }
        }
        can_kick && !could_kick
    }

    /// Someone is in a room: they joined it, or the list of names had them.
    /// A lurker in their own room starts their time there the first time,
    /// and is welcomed if that is because they joined it. Returns the
    /// welcome, if any.
    fn seen(&mut self, room: Room, nick: &str, joined: bool, now: i64) -> Option<Command> {
        let lurker = log_db(self.db.lurker(nick)).flatten()?;
        let channel = self.room_channel(room).to_owned();
        if lurker.room != room {
            if joined {
                let theirs = self.room_channel(lurker.room);
                eprintln!("Lurker {nick}, marked for {theirs}, joined {channel}");
            }
            return None;
        }
        if self.invited.remove(&lurker.id).is_some() {
            eprintln!("Lurker {nick} joined {channel} after being invited");
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
        let facts = Facts {
            lurker: &lurker,
            room: &channel,
            now,
        };
        let (_, welcome) = self
            .messages
            .choose(Kind::Welcome, &facts, &mut self.rng, &[])?;
        eprintln!("Welcomed {nick} to {channel}");
        Some(Command::PRIVMSG(channel, welcome))
    }

    fn departed(&mut self, channel: &str, nick: &str, how: &str) {
        if self.presence.remove(channel, nick) && self.is_lurker(nick) {
            eprintln!("Lurker {nick} {how} {channel}");
        }
    }

    fn lost_channel(&mut self, channel: &str) {
        let key = casemap::fold(channel);
        self.joined.remove(&key);
        self.status.remove(&key);
        self.presence.clear(channel);
    }

    /// Logs how many people and lurkers are in a channel, and for a channel
    /// other than the rooms, whether lurker can kick there.
    fn log_channel(&self, channel: &str) {
        let lurkers = log_db(self.db.lurkers()).map_or(0, |lurkers| {
            lurkers
                .iter()
                .filter(|lurker| self.presence.contains(channel, &lurker.nick))
                .count()
        });
        let count = self.presence.count(channel);
        if self.room_of(channel).is_some() {
            eprintln!("{channel}: {count} people besides lurker, {lurkers} of them lurkers");
        } else {
            let kicking = if self.can_kick(channel) {
                "lurker can warn and kick lurkers here"
            } else {
                "lurker needs +h or +o here to warn and kick lurkers"
            };
            eprintln!(
                "{channel}: {count} people besides lurker, {lurkers} of them lurkers; {kicking}"
            );
        }
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

    /// The rooms and channels lurker should be in.
    fn wanted(&self) -> Vec<String> {
        let mut channels: Vec<String> =
            self.settings.config.rooms.all().map(str::to_owned).to_vec();
        channels.extend(log_db(self.db.channels()).unwrap_or_default());
        channels
    }

    fn room_of(&self, channel: &str) -> Option<Room> {
        self.settings.config.rooms.find(channel)
    }

    fn room_channel(&self, room: Room) -> &str {
        self.settings.config.rooms.channel(room)
    }

    /// Whether lurker has +h or higher in a channel.
    fn can_kick(&self, channel: &str) -> bool {
        self.status
            .get(&casemap::fold(channel))
            .is_some_and(|status| status.iter().any(|&s| can_kick_with(s)))
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
    const HOUR: i64 = 3600;
    const ADMIN: &str = ":rachel!~r@home.example.net";

    fn messages() -> &'static Messages {
        static MESSAGES: OnceLock<Messages> = OnceLock::new();
        MESSAGES.get_or_init(|| Messages::builtin().unwrap())
    }

    fn message(line: &str) -> Message {
        line.parse().unwrap()
    }

    fn join(channel: &str) -> Command {
        Command::JOIN(channel.to_owned(), None, None)
    }

    fn reply(text: &str) -> Command {
        Command::PRIVMSG("rachel".to_owned(), text.to_owned())
    }

    /// The target and text of a message lurker sends.
    fn said(command: &Command) -> (&str, &str) {
        match command {
            Command::PRIVMSG(target, text) => (target.as_str(), text.as_str()),
            other => panic!("expected a PRIVMSG, got {other:?}"),
        }
    }

    /// Feeds raw lines to a session at a time, returning what it sends back.
    fn feed_at(session: &mut Session<'_>, now: i64, lines: &[&str]) -> Vec<Command> {
        lines
            .iter()
            .flat_map(|line| session.handle(&message(line), now))
            .collect()
    }

    fn feed(session: &mut Session<'_>, lines: &[&str]) -> Vec<Command> {
        feed_at(session, 1000, lines)
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

    /// Joins #wetfish as an admin's channel, with `status` (such as `@`)
    /// for lurker, and `others` there too.
    fn in_wetfish(session: &mut Session<'_>, db: &Db, status: &str, others: &str) {
        db.add_channel("#wetfish", "rachel", 0).unwrap();
        let names = format!(":irc.example.net 353 lurker = #wetfish :{status}lurker {others}");
        feed(
            session,
            &[
                ":lurker!lurker@host JOIN #wetfish",
                &names,
                ":irc.example.net 366 lurker #wetfish :End of /NAMES list.",
            ],
        );
    }

    fn mark(db: &Db, nick: &str, room: Room, grace: Option<i64>) -> i64 {
        db.mark(nick, room, false, "rachel", 0).unwrap();
        let id = db.lurker(nick).unwrap().unwrap().id;
        db.set_grace(id, grace).unwrap();
        id
    }

    fn here_since(db: &Db, nick: &str) -> Option<i64> {
        db.lurker(nick).unwrap().unwrap().here_since
    }

    #[test]
    fn waits_for_the_welcome_before_rejoining() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        db.add_channel("#wetfish", "rachel", 0).unwrap();
        let mut session = Session::new(&mut settings, &db, messages());
        assert!(session.rejoin().is_empty());

        feed(&mut session, &[":irc.example.net 001 lurker :Welcome"]);
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist"), join("#wetfish")]
        );
    }

    #[test]
    fn joins_its_channels_when_it_connects() {
        let settings = Settings::example();
        let db = Db::open_in_memory();
        db.add_channel("#wetfish", "rachel", 0).unwrap();
        assert_eq!(
            irc_config(&settings, &db).channels,
            ["#purgatory", "#FBI_watchlist", "#wetfish"]
        );
    }

    #[test]
    fn reads_the_names_in_a_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "alice", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker_");

        feed(
            &mut session,
            &[
                ":irc.example.net 353 lurker_ = #purgatory :lurker_ @op +Alice bob lurker",
                ":irc.example.net 366 lurker_ #purgatory :End of /NAMES list.",
            ],
        );
        assert_eq!(session.presence.count("#purgatory"), 3);
        assert!(session.presence.contains("#purgatory", "alice"));
        assert_eq!(here_since(&db, "alice"), Some(1000));
        assert_eq!(db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn starts_a_lurkers_time_when_they_join_their_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "alice", Room::Purgatory, None);
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
            ],
        );
        assert_eq!(session.presence.count("#purgatory"), 2);

        feed(
            &mut session,
            &[
                ":bob!~b@h PART #purgatory :bye",
                ":op!op@host KICK #FBI_watchlist alice :out",
            ],
        );
        assert_eq!(session.presence.count("#purgatory"), 1);
        assert_eq!(session.presence.count("#FBI_watchlist"), 0);

        feed(&mut session, &[":alice!~a@h QUIT :Ping timeout"]);
        assert_eq!(session.presence.count("#purgatory"), 0);
    }

    #[test]
    fn follows_lurkers_nick_changes() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "alice", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":alice!~a@h NICK alice_"],
        );
        assert!(session.presence.contains("#purgatory", "alice_"));
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
            &[&format!("{ADMIN} PRIVMSG lurker :trusted alice 3d")],
        );
        assert_eq!(
            sent,
            [reply(
                "alice is marked trusted (#purgatory). Kicked from my channels 3 days after \
                 being warned."
            )]
        );
        assert_eq!(here_since(&db, "alice"), Some(1000));
        assert_eq!(db.lurker("alice").unwrap().unwrap().marked_by, "rachel");
    }

    #[test]
    fn answers_admins_who_address_it_in_a_channel() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        for line in [
            "PRIVMSG #purgatory :lurker: untrusted bob 1h",
            "PRIVMSG #purgatory :Lurker, list",
            "PRIVMSG #purgatory :lurker forget bob",
        ] {
            let sent = feed(&mut session, &[&format!("{ADMIN} {line}")]);
            assert!(!sent.is_empty(), "no reply to {line}");
            assert!(
                sent.iter().all(|command| said(command).0 == "#purgatory"
                    && said(command).1.starts_with("rachel: ")),
                "{sent:?}"
            );
        }
        assert_eq!(db.lurker("bob").unwrap(), None);

        // Not from someone who isn't an admin, and not without lurker's nick
        let sent = feed(
            &mut session,
            &[
                ":mallory!~m@h PRIVMSG #purgatory :lurker: untrusted bob 1h",
                &format!("{ADMIN} PRIVMSG #purgatory :lurkers are untrusted"),
                &format!("{ADMIN} PRIVMSG #purgatory :untrusted bob 1h"),
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn reloads_the_settings_for_admins() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(&mut session, &[&format!("{ADMIN} PRIVMSG lurker :reload")]);
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
                ":rachel!~r@elsewhere.net PRIVMSG lurker :untrusted alice 1d",
                ":mallory!~m@home.example.net PRIVMSG lurker :untrusted alice 1d",
                &format!("{ADMIN} PRIVMSG #purgatory :untrusted alice 1d"),
                &format!("{ADMIN} PRIVMSG lurker :\u{1}VERSION\u{1}"),
                ":mallory!~m@h INVITE lurker :#music",
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(db.lurker("alice").unwrap(), None);
        assert!(db.channels().unwrap().is_empty());
    }

    #[test]
    fn joins_channels_admins_invite_it_to() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        let sent = feed(&mut session, &[&format!("{ADMIN} INVITE lurker :#wetfish")]);
        assert_eq!(
            sent,
            [
                join("#wetfish"),
                reply("Joining #wetfish. Give me +h or +o there so I can warn and kick lurkers.")
            ]
        );
        assert_eq!(db.channels().unwrap(), ["#wetfish"]);
        assert_eq!(session.rejoin(), [join("#wetfish")]);

        feed(&mut session, &[":lurker!lurker@host JOIN #wetfish"]);
        assert!(session.rejoin().is_empty());
    }

    #[test]
    fn joins_and_leaves_channels_on_command() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        let sent = feed(
            &mut session,
            &[&format!("{ADMIN} PRIVMSG lurker :join #music")],
        );
        assert_eq!(sent[0], join("#music"));
        feed(&mut session, &[":lurker!lurker@host JOIN #music"]);

        fn replies(session: &mut Session<'_>, text: &str) -> Vec<String> {
            feed(session, &[&format!("{ADMIN} PRIVMSG lurker :{text}")])
                .iter()
                .filter_map(|command| match command {
                    Command::PRIVMSG(_, text) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }
        assert_eq!(
            replies(&mut session, "join #MUSIC"),
            ["I'm already in #MUSIC."]
        );
        assert_eq!(
            replies(&mut session, "join #purgatory"),
            ["#purgatory is one of my rooms, so I'm always in it."]
        );
        assert_eq!(
            replies(&mut session, "channels"),
            ["My channels: #music (needs +h or +o)"]
        );

        let sent = feed(
            &mut session,
            &[&format!("{ADMIN} PRIVMSG lurker :part #music")],
        );
        assert_eq!(
            sent,
            [
                Command::PART("#music".to_owned(), Some(PART_MESSAGE.to_owned())),
                reply("Leaving #music.")
            ]
        );
        assert_eq!(
            replies(&mut session, "part #music"),
            ["#music isn't one of my channels."]
        );
        assert_eq!(
            replies(&mut session, "channels"),
            ["I'm only in my rooms. Invite me to a channel, or send: join <#channel>"]
        );
    }

    #[test]
    fn knows_when_it_can_kick() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        in_wetfish(&mut session, &db, "@%", "bob");
        assert!(session.can_kick("#wetfish"));

        feed(&mut session, &[":op!op@h MODE #wetfish -o lurker"]);
        assert!(session.can_kick("#wetfish"), "still has +h");
        feed(&mut session, &[":op!op@h MODE #wetfish -h lurker"]);
        assert!(!session.can_kick("#wetfish"));
        feed(&mut session, &[":op!op@h MODE #wetfish +v lurker"]);
        assert!(!session.can_kick("#wetfish"));
        feed(&mut session, &[":op!op@h MODE #wetfish +ho lurker bob"]);
        assert!(session.can_kick("#wetfish"));
    }

    #[test]
    fn warns_then_kicks_lurkers_and_invites_them_to_their_room() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        // Joining #wetfish with +o finds bob there and warns him at once
        db.add_channel("#wetfish", "rachel", 0).unwrap();
        let sent = feed_at(
            &mut session,
            2000,
            &[
                ":lurker!lurker@host JOIN #wetfish",
                ":irc.example.net 353 lurker = #wetfish :@lurker bob carol",
                ":irc.example.net 366 lurker #wetfish :End of /NAMES list.",
            ],
        );
        let [warning] = sent.as_slice() else {
            panic!("expected a warning, got {sent:?}");
        };
        let (channel, text) = said(warning);
        assert_eq!(channel, "#wetfish");
        assert!(text.contains("bob") && text.contains("1 HOUR"), "{text}");
        assert_eq!(db.lurker("bob").unwrap().unwrap().warned_at, Some(2000));

        // Nothing more until the grace period is over
        assert!(session.enforce(2000 + HOUR - 1).is_empty());
        let sent = session.enforce(2000 + HOUR);
        let [
            Command::KICK(channel, nick, Some(_)),
            Command::INVITE(invited, room),
        ] = sent.as_slice()
        else {
            panic!("expected a kick and an invite, got {sent:?}");
        };
        assert_eq!((channel.as_str(), nick.as_str()), ("#wetfish", "bob"));
        assert_eq!((invited.as_str(), room.as_str()), ("bob", "#FBI_watchlist"));
        assert!(!session.presence.contains("#wetfish", "bob"));

        // Coming back gets bob kicked again straight away
        let sent = feed_at(&mut session, 2000 + HOUR + 60, &[":bob!~b@h JOIN #wetfish"]);
        assert!(matches!(sent.first(), Some(Command::KICK(..))), "{sent:?}");

        // Admins hear about it when bob hasn't joined his room
        assert!(session.enforce(2000 + HOUR + INVITE_WAIT - 1).is_empty());
        let sent = session.enforce(2000 + HOUR + INVITE_WAIT);
        assert_eq!(
            sent,
            [reply(
                "bob was kicked from #wetfish for lurking and invited to #FBI_watchlist, but \
                 hasn't joined it. Sajoin them: /sajoin bob #FBI_watchlist"
            )]
        );
        assert!(session.enforce(2000 + 2 * HOUR).is_empty());
        assert_eq!(db.lurker("bob").unwrap().unwrap().id, bob);
    }

    #[test]
    fn does_not_ask_for_a_sajoin_when_the_lurker_joins() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        in_wetfish(&mut session, &db, "@", "bob");

        // Warned at 0 with an hour's grace, so kicked from 3600
        let sent = session.enforce(HOUR);
        assert!(matches!(sent.last(), Some(Command::INVITE(..))), "{sent:?}");

        let sent = feed_at(&mut session, HOUR + 10, &[":bob!~b@h JOIN #FBI_watchlist"]);
        assert_eq!(said(&sent[0]).0, "#FBI_watchlist", "a welcome");
        assert!(session.enforce(HOUR + INVITE_WAIT).is_empty());
    }

    #[test]
    fn leaves_lurkers_alone_where_it_cannot_kick_or_without_grace() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "bob", Room::Watchlist, Some(HOUR));
        mark(&db, "carol", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        in_wetfish(&mut session, &db, "+", "bob carol");
        assert!(session.enforce(DAY).is_empty());
        assert_eq!(db.lurker("bob").unwrap().unwrap().warned_at, None);

        // Given +o, it warns bob, but never carol
        let sent = feed(&mut session, &[":op!op@h MODE #wetfish +o lurker"]);
        assert_eq!(sent.len(), 1);
        assert!(said(&sent[0]).1.contains("bob"));
        assert!(
            session
                .enforce(10 * DAY)
                .iter()
                .all(|command| match command {
                    Command::KICK(_, nick, _) => nick == "bob",
                    _ => true,
                })
        );
    }

    #[test]
    fn never_kicks_from_its_rooms() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":irc.example.net 353 lurker = #purgatory :@lurker bob"],
        );
        assert!(session.enforce(DAY).is_empty());
    }

    #[test]
    fn welcomes_lurkers_the_first_time_they_join() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "alice", Room::Watchlist, None);
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
        mark(&db, "alice", Room::Purgatory, None);
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
        mark(&db, "alice", Room::Purgatory, None);
        mark(&db, "bob", Room::Watchlist, None);
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

        let later = feed_at(
            &mut session,
            1000 + DAY,
            &[":alice!~a@h PRIVMSG #purgatory :\u{1}ACTION is back\u{1}"],
        );
        assert_eq!(later.len(), 2);
        assert!(said(&later[1]).1.ends_with("\"/me is back\""));
    }

    #[test]
    fn pings_lurkers_when_their_ping_is_due() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let alice = mark(&db, "alice", Room::Purgatory, None);
        let bob = mark(&db, "bob", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":bob!~b@h JOIN #purgatory"],
        );

        // Their first pings are scheduled 3 to 7 days after joining
        assert!(session.ping_due(2000).is_empty());
        let first = db.lurker("alice").unwrap().unwrap().next_ping_at.unwrap();
        assert!((1000 + 3 * DAY..1000 + 7 * DAY).contains(&first));

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
        let alice = mark(&db, "alice", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(
            &mut session,
            &[":alice!~a@h JOIN #purgatory", ":alice!~a@h QUIT :bye"],
        );
        db.set_next_ping(alice, 1500).unwrap();

        assert!(session.ping_due(2000).is_empty());
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);
        assert_eq!(session.ping_due(3000).len(), 1);
    }

    #[test]
    fn admins_can_poke_lurkers() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        mark(&db, "alice", Room::Purgatory, None);
        mark(&db, "bob", Room::Purgatory, None);
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);

        let sent = feed(
            &mut session,
            &[&format!("{ADMIN} PRIVMSG lurker :poke alice")],
        );
        let [ping, answer] = sent.as_slice() else {
            panic!("expected a ping and a reply, got {sent:?}");
        };
        assert_eq!(said(ping).0, "#purgatory");
        assert_eq!(said(answer), ("rachel", "Poked alice in #purgatory."));

        let sent = feed(
            &mut session,
            &[
                &format!("{ADMIN} PRIVMSG lurker :poke bob"),
                &format!("{ADMIN} PRIVMSG lurker :poke carol"),
                &format!("{ADMIN} PRIVMSG lurker :poke"),
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
        assert_eq!(session.presence.count("#purgatory"), 0);
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
