//! Keeps lurker on IRC: connects over TLS, logs in as an IRC operator, joins
//! its rooms and the channels admins asked it to join, rejoins them when
//! needed, takes its nick back when it was taken, reconnects after a lost
//! connection, and quits with a message when lurker is stopped. Along the
//! way it follows who is in its channels, answers admins, and deals with
//! lurkers: a welcome when they first join their room, fanfare when they
//! speak, a ping about once a week, and, as an IRC operator, a warning and
//! then kicks from every channel and an invite to their room, with a sajoin
//! there if they don't join on the invite. lurker kicks as a channel op
//! rather than with SAKICK, since the Matrix bridge only removes a Matrix
//! user from the matching room for a kick by a user. It also reminds the
//! people on the nudge list to chat, in every channel they are in, and
//! stays in those channels until their nudge ends to hear them answer.

use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::dates;
use crate::db::{Db, Lurker, NickChange, Nudge, NudgeEnd};
use crate::http::Status;
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

/// How often to check on lurkers: pings, warnings and kicks that are due,
/// and which lurkers are online.
const LURKER_INTERVAL: Duration = Duration::from_secs(60);

/// After making a fuss about a lurker speaking, lurker stays quiet about
/// them for this many seconds, so a lurker who keeps talking isn't drowned
/// in fanfare and admins aren't flooded with notices.
const FANFARE_COOLDOWN: i64 = 24 * 3600;

/// How often lurker looks up a lurker whose grace period is over, to kick
/// them from channels they joined since. Joining one of lurker's channels
/// gets them kicked straight away.
const SWEEP_INTERVAL: i64 = 5 * 60;

/// How long lurker waits before giving itself +o again in a channel, so it
/// never fights someone over it.
const OP_RETRY: i64 = 10 * 60;

/// How many seconds lurker waits after inviting a lurker to their room
/// before sajoining them there, picked at random each time. Long enough to
/// tell a client that joins on an invite from a sajoin.
const INVITE_WAIT: std::ops::RangeInclusive<i64> = 30..=60;

/// How many seconds lurker waits after kicking a lurker from their room
/// before inviting them back. The Matrix bridge is slow to pass on the kick,
/// and withdraws an invite that reaches it before the kick does.
const REINVITE_DELAY: i64 = 60;

/// How long lurker waits before looking someone on the nudge list up again,
/// when a reminder couldn't go out, such as because it sent another in one
/// of their channels a moment ago.
const NUDGE_RETRY: i64 = 5 * 60;

/// How often to check for invites and sajoins that are due.
const INVITE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The most nicks lurker asks about in one ISON, in bytes.
const ISON_LENGTH: usize = 400;

/// The wait before reconnecting, doubled after each attempt up to
/// `MAX_RETRY_DELAY`.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// A connection that lasted this long counts as having worked, so the wait
/// before reconnecting starts again from `FIRST_RETRY_DELAY`.
const STABLE_CONNECTION: Duration = Duration::from_secs(300);

/// Stays connected, reconnecting whenever the connection is lost, until
/// lurker is stopped.
pub async fn run(
    settings: &mut Settings,
    db: &Db,
    messages: &Messages,
    status: &Status,
    shutdown: &mut Shutdown,
) {
    let mut delay = FIRST_RETRY_DELAY;

    loop {
        let started = Instant::now();

        let result = connect(settings, db, messages, status, shutdown).await;
        status.set_connected(false);
        match result {
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
    status: &Status,
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
    // lurker knows whether it is still opped after a mode change
    client
        .send_cap_req(&[Capability::MultiPrefix])
        .map_err(|err| describe(&err))?;
    client.identify().map_err(|err| describe(&err))?;
    let mut stream = client.stream().map_err(|err| describe(&err))?;
    let mut session = Session::new(settings, db, messages);
    serve(&client, &mut stream, &mut session, status, shutdown).await
}

/// Handles messages, rejoins channels, rereads the settings and deals with
/// lurkers until the connection is lost or lurker is stopped.
async fn serve(
    client: &Client,
    stream: &mut ClientStream,
    session: &mut Session<'_>,
    status: &Status,
    shutdown: &mut Shutdown,
) -> Result<(), String> {
    let mut rejoin = time::interval(REJOIN_INTERVAL);
    rejoin.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut settings = time::interval(SETTINGS_INTERVAL);
    settings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut lurkers = time::interval(LURKER_INTERVAL);
    lurkers.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut invites = time::interval(INVITE_CHECK_INTERVAL);
    invites.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
            _ = lurkers.tick() => session.tick(unix_now()),
            _ = invites.tick() => session.invites_due(unix_now()),
            () = shutdown.recv() => {
                quit(client, stream).await;
                return Ok(());
            }
        };

        for command in commands {
            client.send(command).map_err(|err| describe(&err))?;
        }
        // Connected once the server has accepted the connection
        status.set_connected(session.registered);
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

/// Whether a channel status is +o or higher.
fn is_op(status: char) -> bool {
    "qao".contains(status)
}

/// Removes the entries whose time has come from a map of lurker ids to
/// times, and returns their ids.
fn take_due(times: &mut HashMap<i64, i64>, now: i64) -> HashSet<i64> {
    let due: HashSet<i64> = times
        .iter()
        .filter(|&(_, &at)| at <= now)
        .map(|(&id, _)| id)
        .collect();
    times.retain(|id, _| !due.contains(id));
    due
}

/// The channels a WHOIS reply lists, without the status prefixes in front
/// of them, such as `@` in `@#wetfish`. `&` is a status prefix too, so only
/// `#` starts a channel name.
fn whois_channels(list: &str) -> impl Iterator<Item = &str> {
    list.split_whitespace()
        .filter_map(|entry| entry.find('#').map(|start| &entry[start..]))
}

/// What a WHOIS lurker asked for has said so far.
#[derive(Default)]
struct Lookup {
    /// Whether the server knows the nick
    found: bool,
    /// The channels they are in
    channels: Vec<String>,
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
    /// Whether lurker is logged in as an IRC operator, which it needs to
    /// warn and kick lurkers
    opered: bool,
    /// The channels lurker is in, folded
    joined: HashSet<String>,
    /// lurker's own status in each channel, such as `o` for +o, by folded
    /// channel name
    status: HashMap<String, HashSet<char>>,
    /// When lurker last gave itself +o in each channel, by folded name
    op_asked: HashMap<String, i64>,
    /// Who else is in them
    presence: Presence,
    /// When lurker last pinged someone in each room, to space pings out
    last_ping: HashMap<Room, i64>,
    /// The welcome lurker last sent in each room, by its number in
    /// messages.toml, so the next one is different
    last_welcome: HashMap<Room, usize>,
    /// Folded nicks of lurkers the server last said were online
    online: HashSet<String>,
    /// The nicks in each ISON sent, oldest first, to match the replies to
    ison: VecDeque<Vec<String>>,
    /// The WHOIS replies being collected, by folded nick
    lookups: HashMap<String, Lookup>,
    /// When lurker last kicked each lurker from their channels, by id
    swept: HashMap<i64, i64>,
    /// Channels lurker sajoined itself into to kick someone, folded, with
    /// how many times it is still to leave each
    visiting: HashMap<String, usize>,
    /// Lurkers invited to their room, by id, with when lurker sajoins them
    /// there unless they join first
    invited: HashMap<i64, i64>,
    /// Lurkers kicked from their room to be invited back, by id, with when
    /// lurker invites them
    reinvite: HashMap<i64, i64>,
    /// When lurker last looked up each nudge's nick, by nudge id
    nudge_checked: HashMap<i64, i64>,
    /// When lurker last sent a nudge reminder in each channel, folded
    nudge_posted: HashMap<String, i64>,
    /// Channels lurker is in only for nudges, folded, with their names, so
    /// it can leave them once no nudge needs them
    nudge_joined: HashMap<String, String>,
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
            opered: false,
            joined: HashSet::new(),
            status: HashMap::new(),
            op_asked: HashMap::new(),
            presence: Presence::default(),
            last_ping: HashMap::new(),
            last_welcome: HashMap::new(),
            online: HashSet::new(),
            ison: VecDeque::new(),
            lookups: HashMap::new(),
            swept: HashMap::new(),
            visiting: HashMap::new(),
            invited: HashMap::new(),
            reinvite: HashMap::new(),
            nudge_checked: HashMap::new(),
            nudge_posted: HashMap::new(),
            nudge_joined: HashMap::new(),
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
                commands.extend(self.oper_up());
            }

            // Logging in as an IRC operator
            Command::Response(Response::RPL_YOUREOPER, _) => {
                self.opered = true;
                eprintln!("Logged in as an IRC operator");
                commands.extend(self.op_myself(now));
                commands.extend(self.enforce(now));
            }
            Command::Response(
                response @ (Response::ERR_PASSWDMISMATCH | Response::ERR_NOOPERHOST),
                args,
            ) => {
                commands.extend(self.oper_failed(*response, args.last()));
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
                    commands.extend(self.op_myself(now));
                    commands.extend(self.enforce(now));
                }
            }

            // Which lurkers are online, and where
            Command::Response(Response::RPL_ISON, args) => {
                self.ison_reply(args.last().map_or("", String::as_str));
                commands.extend(self.enforce(now));
                commands.extend(self.look_up_nudged(now));
            }
            Command::Response(Response::RPL_WHOISUSER, args) => {
                if let Some(nick) = args.get(1)
                    && let Some(lookup) = self.lookup(nick)
                {
                    lookup.found = true;
                }
            }
            Command::Response(Response::RPL_WHOISCHANNELS, args) => {
                if let [_, nick, list, ..] = args.as_slice()
                    && let Some(lookup) = self.lookup(nick)
                {
                    lookup
                        .channels
                        .extend(whois_channels(list).map(str::to_owned));
                }
            }
            Command::Response(Response::ERR_NOSUCHNICK, args)
                if args
                    .get(1)
                    .is_some_and(|nick| self.lookups.contains_key(&casemap::fold(nick))) => {}
            Command::Response(Response::RPL_ENDOFWHOIS, args) => {
                if let Some(nick) = args.get(1) {
                    commands.extend(self.looked_up(nick, now));
                }
            }

            Command::JOIN(channels, _, _) => {
                for channel in channels.split(',') {
                    if self.is_me(source) {
                        self.joined.insert(casemap::fold(channel));
                        if self.is_nudge_channel(channel) && !self.is_wanted(channel) {
                            self.nudge_joined
                                .insert(casemap::fold(channel), channel.to_owned());
                        }
                        eprintln!("Joined {channel}");
                        continue;
                    }
                    self.presence.add(channel, source);
                    self.joined_on_invite(channel, source);
                    if let Some(room) = self.room_of(channel) {
                        commands.extend(self.seen(room, source, true, now));
                    }
                    commands.extend(self.arrived(channel, source, now));
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
                self.online.remove(&casemap::fold(source));
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
                if self.online.remove(&casemap::fold(source)) {
                    self.online.insert(casemap::fold(new_nick));
                }
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
                    commands.extend(self.op_myself(now));
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
            // What the server says about lurker's own commands, such as a
            // SAJOIN it wasn't allowed
            Command::NOTICE(target, text) if self.is_me(target) && source.is_empty() => {
                eprintln!("Server notice: {text}");
            }

            Command::ERROR(text) => eprintln!("Server error: {text}"),
            Command::Response(response, args) if (400..600).contains(&(*response as u16)) => {
                eprintln!("warning: {response:?} {}", args.join(" "));
            }
            _ => {}
        }

        commands
    }

    /// What lurker does every minute: sends pings that are due, gives itself
    /// +o where it lost it, asks which lurkers and nudged people are online,
    /// warns and kicks the lurkers that are due, ends the nudges whose time
    /// is up, and leaves the channels no nudge needs any more.
    fn tick(&mut self, now: i64) -> Vec<Command> {
        let mut commands = self.ping_due(now);
        commands.extend(self.op_myself(now));
        commands.extend(self.check_online(now));
        commands.extend(self.enforce(now));
        commands.extend(self.end_nudges(now));
        commands.extend(self.look_up_nudged(now));
        commands.extend(self.leave_nudge_channels());
        commands
    }

    /// JOIN commands for the rooms and channels lurker is not in, once the
    /// server has accepted the connection.
    fn rejoin(&self) -> Vec<Command> {
        if !self.registered {
            return Vec::new();
        }

        let mut commands: Vec<Command> = self
            .wanted()
            .into_iter()
            .filter(|channel| !self.joined.contains(&casemap::fold(channel)))
            .map(|channel| Command::JOIN(channel, None, None))
            .collect();
        // The channels it stays in for nudges, which it may not be allowed
        // to join any other way
        if self.opered {
            for channel in log_db(self.db.nudge_channels()).unwrap_or_default() {
                if !self.joined.contains(&casemap::fold(&channel)) && !self.is_wanted(&channel) {
                    commands.push(Command::SAJOIN(self.nick.clone(), channel));
                }
            }
        }
        commands
    }

    /// Reads the settings file again if it changed, and logs what happened.
    fn check_settings(&mut self) {
        match self.settings.reload(false) {
            Some(Ok(summary)) => eprintln!("{summary}"),
            Some(Err(err)) => eprintln!("error: {err}"),
            None => {}
        }
    }

    /// OPER with the login in the settings, if there is one.
    fn oper_up(&self) -> Option<Command> {
        let Some(oper) = &self.settings.config.oper else {
            eprintln!("No [oper] in the settings, so lurker won't warn or kick anyone");
            return None;
        };
        eprintln!("Logging in as IRC operator {}", oper.name);
        Some(Command::OPER(oper.name.clone(), oper.password.clone()))
    }

    /// Logs a refused OPER, and tells the admins, since lurker can't warn or
    /// kick anyone without it.
    fn oper_failed(&self, response: Response, text: Option<&String>) -> Vec<Command> {
        let reason = text.map_or_else(|| format!("{response:?}"), String::clone);
        eprintln!("error: the server refused lurker's IRC operator login: {reason}");
        let notice = format!(
            "I couldn't log in as an IRC operator ({reason}), so I can't op myself, kick or \
             sajoin lurkers. Check [oper] in lurker.toml and the oper block on the IRC server."
        );
        self.settings
            .config
            .admins
            .iter()
            .map(|admin| Command::PRIVMSG(admin.nick.clone(), notice.clone()))
            .collect()
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

    /// A message in a channel. An admin starting it with lurker's nick and a
    /// command's name gives a command, answered in the channel. Anything
    /// else after lurker's nick is ordinary chat and gets no reply. A
    /// message may also be a lurker speaking, which stops their kicks, and
    /// in their room plays the fanfare, or someone being nudged speaking,
    /// which ends their nudge.
    fn channel_message(
        &mut self,
        message: &Message,
        channel: &str,
        text: &str,
        now: i64,
    ) -> Vec<Command> {
        if let Some(command) = self.addressed(text)
            && command
                .split_whitespace()
                .next()
                .is_some_and(admin::is_command)
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
        let mut commands = self.heard(channel, source, text, now);
        if let Some(room) = self.room_of(channel) {
            commands.extend(self.spoke(room, source, text, now));
        }
        commands
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
                let lines = admin::run(&context, text);
                // Marking someone may make a warning due
                (self.enforce(now), lines)
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
            vec![format!("Joining {channel}.")],
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

    /// `channels`: the channels admins asked lurker to join, whether it is
    /// opped in each, and whether it is an IRC operator.
    fn channels_report(&self) -> String {
        let oper = if self.opered {
            "I'm logged in as an IRC operator, so I warn and kick lurkers."
        } else {
            "I'm not logged in as an IRC operator, so I don't warn or kick anyone."
        };
        let channels = log_db(self.db.channels()).unwrap_or_default();
        if channels.is_empty() {
            return format!(
                "I'm only in my rooms. Invite me to a channel, or send: join <#channel>. {oper}"
            );
        }
        let described: Vec<String> = channels
            .iter()
            .map(|channel| {
                let state = if !self.joined.contains(&casemap::fold(channel)) {
                    "not in it right now"
                } else if self.has_op(channel) {
                    "opped"
                } else {
                    "not opped"
                };
                format!("{channel} ({state})")
            })
            .collect();
        format!("My channels: {}. {oper}", described.join(", "))
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

    /// Lurkers lurker warns and kicks: those with a grace period who haven't
    /// spoken since their warning, apart from lurker itself.
    fn enforceable(&self, lurker: &Lurker) -> bool {
        lurker.grace.is_some() && !lurker.released() && !self.is_own_nick(&lurker.nick)
    }

    /// Whether lurker needs to know if a lurker is online: to warn them, or
    /// once their grace period is over, to kick them.
    fn wanted_online(&self, lurker: &Lurker, now: i64) -> bool {
        self.enforceable(lurker) && lurker.kick_at().is_none_or(|at| at <= now)
    }

    /// ISON for the lurkers lurker needs to know are online, and the people
    /// being nudged who have a reminder due, as an IRC operator. The
    /// replies come back in the same order.
    fn check_online(&mut self, now: i64) -> Vec<Command> {
        if !self.opered {
            return Vec::new();
        }
        let Some(lurkers) = log_db(self.db.lurkers()) else {
            return Vec::new();
        };
        let mut nicks: Vec<String> = lurkers
            .iter()
            .filter(|lurker| self.wanted_online(lurker, now))
            .map(|lurker| lurker.nick.clone())
            .collect();
        nicks.extend(
            log_db(self.db.going_nudges())
                .unwrap_or_default()
                .into_iter()
                .filter(|nudge| nudge.reminder_due(now))
                .map(|nudge| nudge.nick),
        );

        let mut batches: Vec<Vec<String>> = Vec::new();
        let mut length = 0;
        for nick in nicks {
            if batches.is_empty() || length + nick.len() >= ISON_LENGTH {
                batches.push(Vec::new());
                length = 0;
            }
            length += nick.len() + 1;
            if let Some(batch) = batches.last_mut() {
                batch.push(nick);
            }
        }

        self.ison.extend(batches.iter().cloned());
        batches.into_iter().map(Command::ISON).collect()
    }

    /// Notes which nicks an ISON reply says are online.
    fn ison_reply(&mut self, list: &str) {
        let Some(asked) = self.ison.pop_front() else {
            return;
        };
        let online: HashSet<String> = list.split_whitespace().map(casemap::fold).collect();
        for nick in asked {
            let nick = casemap::fold(&nick);
            if online.contains(&nick) {
                self.online.insert(nick);
            } else {
                self.online.remove(&nick);
            }
        }
    }

    /// Whether a nick is online, as far as lurker knows: in one of its
    /// channels, or listed by the last ISON.
    fn is_online(&self, nick: &str) -> bool {
        self.presence.anywhere(nick) || self.online.contains(&casemap::fold(nick))
    }

    /// Deals with every lurker who has a grace period, while lurker is an
    /// IRC operator: warns those who are online and haven't been warned,
    /// and looks up where those whose grace period is over are, to kick
    /// them from there.
    fn enforce(&mut self, now: i64) -> Vec<Command> {
        if !self.opered {
            return Vec::new();
        }
        let Some(lurkers) = log_db(self.db.lurkers()) else {
            return Vec::new();
        };

        let due: Vec<Lurker> = lurkers
            .into_iter()
            .filter(|lurker| self.enforceable(lurker))
            .collect();
        let mut commands = Vec::new();
        for lurker in &due {
            match lurker.kick_at() {
                None => commands.extend(self.warn(lurker, now)),
                Some(at) if at <= now => commands.extend(self.look_up(lurker, now)),
                Some(_) => {}
            }
        }
        commands
    }

    /// Warns a lurker that they will be kicked, in a private message so
    /// nobody else sees it, once they are online. A lurker with a grace
    /// period of 0 isn't warned: lurker only notes when it found them
    /// online, and looks them up to kick them straight away.
    fn warn(&mut self, lurker: &Lurker, now: i64) -> Vec<Command> {
        if !self.is_online(&lurker.nick) {
            return Vec::new();
        }
        if lurker.grace == Some(0) {
            log_db(self.db.set_warned(lurker.id, now));
            eprintln!("Kicking {} without a warning", lurker.nick);
            return self.look_up(lurker, now).into_iter().collect();
        }

        let room = self.room_channel(lurker.room).to_owned();
        let facts = Facts {
            lurker,
            room: &room,
            now,
        };
        let Some((_, warning)) = self
            .messages
            .choose(Kind::Warning, &facts, &mut self.rng, &[])
        else {
            return Vec::new();
        };
        log_db(self.db.set_warned(lurker.id, now));
        eprintln!(
            "Warned {} in a private message; kicked in {}",
            lurker.nick,
            dates::length(lurker.grace.unwrap_or_default())
        );
        vec![Command::PRIVMSG(lurker.nick.clone(), warning)]
    }

    /// WHOIS for a lurker whose grace period is over, to find every channel
    /// they are in, unless lurker swept them a moment ago or they are
    /// offline.
    fn look_up(&mut self, lurker: &Lurker, now: i64) -> Option<Command> {
        let key = casemap::fold(&lurker.nick);
        let recent = self
            .swept
            .get(&lurker.id)
            .is_some_and(|&at| now - at < SWEEP_INTERVAL);
        if self.lookups.contains_key(&key) || recent || !self.is_online(&lurker.nick) {
            return None;
        }
        self.lookups.insert(key, Lookup::default());
        Some(Command::WHOIS(None, lurker.nick.clone()))
    }

    /// The WHOIS lurker is collecting for a nick, if it asked for one.
    fn lookup(&mut self, nick: &str) -> Option<&mut Lookup> {
        self.lookups.get_mut(&casemap::fold(nick))
    }

    /// The end of a WHOIS lurker asked for: kicks the lurker from every
    /// channel they are in, if they are still due.
    fn looked_up(&mut self, nick: &str, now: i64) -> Vec<Command> {
        let Some(lookup) = self.lookups.remove(&casemap::fold(nick)) else {
            return Vec::new();
        };
        if !lookup.found {
            self.online.remove(&casemap::fold(nick));
            return Vec::new();
        }
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return match log_db(self.db.nudge(nick)).flatten() {
                Some(nudge) => self.remind(&nudge, &lookup.channels, now),
                None => Vec::new(),
            };
        };
        if !self.enforceable(&lurker) || lurker.kick_at().is_none_or(|at| at > now) {
            return Vec::new();
        }
        self.sweep(&lurker, &lookup.channels, now)
    }

    /// WHOIS for the people being nudged who have a reminder due and are
    /// online, to find every channel they are in, as an IRC operator.
    fn look_up_nudged(&mut self, now: i64) -> Vec<Command> {
        if !self.opered {
            return Vec::new();
        }
        let mut commands = Vec::new();
        for nudge in log_db(self.db.going_nudges()).unwrap_or_default() {
            let key = casemap::fold(&nudge.nick);
            let recent = self
                .nudge_checked
                .get(&nudge.id)
                .is_some_and(|&at| now - at < NUDGE_RETRY);
            if !nudge.reminder_due(now)
                || recent
                || self.lookups.contains_key(&key)
                || !self.is_online(&nudge.nick)
                || self.is_own_nick(&nudge.nick)
            {
                continue;
            }
            self.nudge_checked.insert(nudge.id, now);
            self.lookups.insert(key, Lookup::default());
            commands.push(Command::WHOIS(None, nudge.nick.clone()));
        }
        commands
    }

    /// Sends a nudge's reminder in every channel they are in, sajoining
    /// itself into those it isn't in, where it stays until the nudge ends.
    /// The reminder waits when lurker sent another nudge's reminder in one
    /// of their channels within the last 20 minutes, so a channel is never
    /// flooded.
    fn remind(&mut self, nudge: &Nudge, channels: &[String], now: i64) -> Vec<Command> {
        if !self.opered || !nudge.reminder_due(now) {
            return Vec::new();
        }
        if channels.is_empty() {
            eprintln!(
                "{} is online but in no channels to remind them in",
                nudge.nick
            );
            return Vec::new();
        }
        let busy = channels.iter().find(|channel| {
            self.nudge_posted
                .get(&casemap::fold(channel))
                .is_some_and(|&at| now - at < schedule::ROOM_GAP)
        });
        if let Some(channel) = busy {
            eprintln!(
                "Reminding {} later, since {channel} had a nudge reminder a moment ago",
                nudge.nick
            );
            return Vec::new();
        }

        let mut commands = Vec::new();
        for channel in channels {
            let key = casemap::fold(channel);
            log_db(self.db.add_nudge_channel(nudge.id, channel));
            let inside = self.joined.contains(&key) && !self.visiting.contains_key(&key);
            if !inside {
                commands.push(Command::SAJOIN(self.nick.clone(), channel.clone()));
            }
            let text = self.messages.reminder(nudge, channel);
            commands.push(Command::PRIVMSG(channel.clone(), text));
            self.nudge_posted.insert(key, now);
        }
        log_db(self.db.set_reminded(nudge.id, now));
        eprintln!(
            "Reminded {} to chat in {}: reminder {} of {}",
            nudge.nick,
            channels.join(", "),
            nudge.sent + 1,
            nudge.reminders
        );
        commands
    }

    /// Ends the nudges whose time is up, and tells the admins, for them to
    /// review.
    fn end_nudges(&mut self, now: i64) -> Vec<Command> {
        let mut commands = Vec::new();
        for nudge in log_db(self.db.going_nudges()).unwrap_or_default() {
            if now < nudge.ends_at() {
                continue;
            }
            if log_db(
                self.db
                    .end_nudge(&nudge.nick, NudgeEnd::Finished, None, now),
            )
            .flatten()
            .is_none()
            {
                continue;
            }
            self.nudge_checked.remove(&nudge.id);
            eprintln!(
                "{}'s nudge is over, after {} of {} reminders",
                nudge.nick, nudge.sent, nudge.reminders
            );
            commands.extend(self.tell_admins(&messages::nudge_over_notice(&nudge)));
        }
        commands
    }

    /// Leaves the channels lurker was in only for nudges that have ended.
    fn leave_nudge_channels(&mut self) -> Vec<Command> {
        let needed: HashSet<String> = log_db(self.db.nudge_channels())
            .unwrap_or_default()
            .iter()
            .chain(self.wanted().iter())
            .map(|channel| casemap::fold(channel))
            .collect();
        let done: Vec<(String, String)> = self
            .nudge_joined
            .iter()
            .filter(|(key, _)| !needed.contains(*key))
            .map(|(key, name)| (key.clone(), name.clone()))
            .collect();

        let mut commands = Vec::new();
        for (key, channel) in done {
            self.nudge_joined.remove(&key);
            if self.joined.contains(&key) {
                eprintln!("Leaving {channel}, since no nudge needs it any more");
                commands.push(Command::PART(channel, None));
            }
        }
        commands
    }

    /// A private message to every admin.
    fn tell_admins(&self, text: &str) -> Vec<Command> {
        self.settings
            .config
            .admins
            .iter()
            .map(|admin| Command::PRIVMSG(admin.nick.clone(), text.to_owned()))
            .collect()
    }

    /// Kicks a lurker from every channel but their room, and invites them
    /// to their room if they aren't in it. A lurker in their room who was
    /// never invited, because lurker sajoined them there before it sent
    /// invites, is kicked from it too and invited back a minute later, so a
    /// Matrix user gets the invite.
    fn sweep(&mut self, lurker: &Lurker, channels: &[String], now: i64) -> Vec<Command> {
        let room = self.room_channel(lurker.room).to_owned();
        let (in_room, others): (Vec<&String>, Vec<&String>) = channels
            .iter()
            .partition(|channel| casemap::eq(channel, &room));
        let mut commands = Vec::new();

        for channel in &others {
            let facts = Facts {
                lurker,
                room: &room,
                now,
            };
            let reason = self
                .messages
                .choose(Kind::Kick, &facts, &mut self.rng, &[])
                .map(|(_, reason)| reason)
                .unwrap_or_default();
            commands.extend(self.kick(channel, &lurker.nick, reason, now));
            self.presence.remove(channel, &lurker.nick);
        }
        if !others.is_empty() {
            let list: Vec<&str> = others.iter().map(|channel| channel.as_str()).collect();
            eprintln!("Kicked {} from {}", lurker.nick, list.join(", "));
        }

        if !in_room.is_empty() && lurker.invited_at.is_none() {
            let facts = Facts {
                lurker,
                room: &room,
                now,
            };
            let reason = self
                .messages
                .choose(Kind::Kick, &facts, &mut self.rng, &[])
                .map(|(_, reason)| reason)
                .unwrap_or_default();
            commands.extend(self.kick(&room, &lurker.nick, reason, now));
            self.presence.remove(&room, &lurker.nick);
            self.reinvite.insert(lurker.id, now + REINVITE_DELAY);
            eprintln!(
                "Kicked {} from {room} to invite them back in {REINVITE_DELAY} seconds, since \
                 they were never invited",
                lurker.nick
            );
        } else if in_room.is_empty() {
            commands.extend(self.invite(lurker, &room, now));
        }

        self.swept.insert(lurker.id, now);
        commands
    }

    /// Invites a lurker to their room, and sajoins them there 30 to 60
    /// seconds later unless they join first. The bridge passes the invite
    /// on to Matrix users, and the wait shows in the log whose client joins
    /// on an invite and who had to be sajoined.
    ///
    /// Inviting needs +o in the room, so lurker ops itself there first if it
    /// isn't opped. Each invite is recorded in the database.
    fn invite(&mut self, lurker: &Lurker, room: &str, now: i64) -> Vec<Command> {
        // Already invited, or waiting to be invited back after a kick
        if self.invited.contains_key(&lurker.id) || self.reinvite.contains_key(&lurker.id) {
            return Vec::new();
        }
        let mut commands = Vec::new();
        let key = casemap::fold(room);
        // Unless it is already opped, or asked for +o a moment ago to kick
        if !self.has_op(room) && self.op_asked.get(&key) != Some(&now) {
            self.op_asked.insert(key, now);
            commands.push(Command::SAMODE(
                room.to_owned(),
                "+o".to_owned(),
                Some(self.nick.clone()),
            ));
        }
        let (shortest, longest) = (*INVITE_WAIT.start(), *INVITE_WAIT.end());
        let wait = shortest + self.rng.below((longest - shortest + 1) as u64) as i64;
        self.invited.insert(lurker.id, now + wait);
        log_db(self.db.set_invited(lurker.id, now));
        eprintln!(
            "Invited {} to {room}; sajoining them in {wait} seconds unless they join",
            lurker.nick
        );
        commands.push(Command::INVITE(lurker.nick.clone(), room.to_owned()));
        commands
    }

    /// Notes an invited lurker joining their room, so they aren't sajoined.
    fn joined_on_invite(&mut self, channel: &str, nick: &str) {
        if self.invited.is_empty() {
            return;
        }
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return;
        };
        if self.room_of(channel) == Some(lurker.room) && self.invited.remove(&lurker.id).is_some() {
            eprintln!("{nick} joined {channel} on the invite");
        }
    }

    /// Invites lurkers kicked from their room once their wait is over, and
    /// sajoins invited lurkers who haven't joined their room in time. Only
    /// lurkers who are still being kicked are invited or sajoined.
    fn invites_due(&mut self, now: i64) -> Vec<Command> {
        let to_invite = take_due(&mut self.reinvite, now);
        let to_sajoin = take_due(&mut self.invited, now);
        if to_invite.is_empty() && to_sajoin.is_empty() {
            return Vec::new();
        }
        let Some(lurkers) = log_db(self.db.lurkers()) else {
            return Vec::new();
        };

        let mut commands = Vec::new();
        for lurker in &lurkers {
            let room = self.room_channel(lurker.room).to_owned();
            let here = self.presence.contains(&room, &lurker.nick);
            if !self.enforceable(lurker) {
                continue;
            }
            if to_invite.contains(&lurker.id) {
                if here {
                    // Back in the room already, so there is nothing to invite
                    // them to
                    log_db(self.db.set_invited(lurker.id, now));
                } else {
                    commands.extend(self.invite(lurker, &room, now));
                }
            }
            if to_sajoin.contains(&lurker.id) && !here {
                eprintln!(
                    "Sajoined {} into {room}, since they didn't join on the invite",
                    lurker.nick
                );
                commands.push(Command::SAJOIN(lurker.nick.clone(), room));
            }
        }
        commands
    }

    /// The commands that kick someone from a channel as a channel op. The
    /// Matrix bridge removes a Matrix user from the matching room for a kick
    /// by a user, but not for a SAKICK, which comes from the server.
    ///
    /// In a channel lurker isn't in, it sajoins itself, which gets past
    /// bans, keys, invite-only and limits, ops itself, kicks, and leaves
    /// again, unless the channel is one of its own. Where it isn't opped, it
    /// ops itself for the kick, and keeps +o.
    fn kick(&mut self, channel: &str, nick: &str, reason: String, now: i64) -> Vec<Command> {
        let key = casemap::fold(channel);
        let inside = self.joined.contains(&key) && !self.visiting.contains_key(&key);
        let kick = Command::KICK(channel.to_owned(), nick.to_owned(), Some(reason));
        if inside && self.has_op(channel) {
            return vec![kick];
        }

        let me = self.nick.clone();
        let own = self.is_wanted(channel) || self.is_nudge_channel(channel);
        let mut commands = Vec::new();
        if !inside {
            commands.push(Command::SAJOIN(me.clone(), channel.to_owned()));
        }
        self.op_asked.insert(key.clone(), now);
        commands.push(Command::SAMODE(
            channel.to_owned(),
            "+o".to_owned(),
            Some(me),
        ));
        commands.push(kick);
        if !own {
            commands.push(Command::PART(channel.to_owned(), None));
            *self.visiting.entry(key).or_default() += 1;
        }
        commands
    }

    /// Someone joined one of lurker's channels. A lurker whose grace period
    /// is over is kicked straight away, unless it is their own room.
    fn arrived(&mut self, channel: &str, nick: &str, now: i64) -> Vec<Command> {
        let Some(lurker) = log_db(self.db.lurker(nick)).flatten() else {
            return Vec::new();
        };
        if self.room_of(channel) == Some(lurker.room) {
            return Vec::new();
        }
        if lurker.kick_at().is_some_and(|at| at <= now) {
            self.swept.remove(&lurker.id);
        }
        self.enforce(now)
    }

    /// Someone said something in one of lurker's channels. A lurker who
    /// speaks after their warning is no longer kicked. Someone being nudged
    /// who speaks ends their nudge, and the admins are told.
    fn heard(&mut self, channel: &str, nick: &str, text: &str, now: i64) -> Vec<Command> {
        if let Some(lurker) = log_db(self.db.lurker(nick)).flatten() {
            log_db(self.db.set_spoke(lurker.id, now));
            if lurker.warned_at.is_some() && !lurker.released() {
                self.swept.remove(&lurker.id);
                eprintln!("Lurker {nick} spoke in {channel}, so they won't be kicked any more");
            }
            return Vec::new();
        }

        let Some(nudge) = log_db(self.db.nudge_spoke(nick, now)).flatten() else {
            return Vec::new();
        };
        self.nudge_checked.remove(&nudge.id);
        eprintln!(
            "{nick} spoke in {channel} after {} of {} nudge reminders, so their nudge ended",
            nudge.sent, nudge.reminders
        );
        self.tell_admins(&messages::nudge_spoke_notice(&nudge, channel, text, now))
    }

    /// Gives lurker +o, with SAMODE as an IRC operator, in each of its
    /// rooms and channels that it isn't opped in. It needs +o in its rooms
    /// to invite lurkers there.
    fn op_myself(&mut self, now: i64) -> Vec<Command> {
        if !self.opered {
            return Vec::new();
        }
        let mut channels: Vec<String> = self
            .settings
            .config
            .rooms
            .all()
            .into_iter()
            .filter(|room| self.joined.contains(&casemap::fold(room)))
            .map(str::to_owned)
            .collect();
        channels.extend(self.patrolled());
        let mut commands = Vec::new();
        for channel in channels {
            let key = casemap::fold(&channel);
            let recent = self
                .op_asked
                .get(&key)
                .is_some_and(|&at| now - at < OP_RETRY);
            if self.has_op(&channel) || recent {
                continue;
            }
            self.op_asked.insert(key, now);
            eprintln!("Opping myself in {channel}");
            commands.push(Command::SAMODE(
                channel,
                "+o".to_owned(),
                Some(self.nick.clone()),
            ));
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

    /// Follows a mode change in a channel. Returns true when lurker lost its
    /// +o there.
    fn mode_changed(&mut self, channel: &str, modes: &[Mode<ChannelMode>]) -> bool {
        let key = casemap::fold(channel);
        let had_op = self.has_op(channel);

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

        had_op && !self.has_op(channel)
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
        // Never the same welcome twice in a row in a room
        let last: Vec<usize> = self.last_welcome.get(&room).copied().into_iter().collect();
        let (index, welcome) = self
            .messages
            .choose(Kind::Welcome, &facts, &mut self.rng, &last)?;
        self.last_welcome.insert(room, index);
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
        if let Some(count) = self.visiting.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.visiting.remove(&key);
            }
        }
        self.joined.remove(&key);
        self.nudge_joined.remove(&key);
        self.status.remove(&key);
        self.presence.clear(channel);
    }

    /// Logs how many people and lurkers are in a channel.
    fn log_channel(&self, channel: &str) {
        let lurkers = log_db(self.db.lurkers()).map_or(0, |lurkers| {
            lurkers
                .iter()
                .filter(|lurker| self.presence.contains(channel, &lurker.nick))
                .count()
        });
        let count = self.presence.count(channel);
        eprintln!("{channel}: {count} people besides lurker, {lurkers} of them lurkers");
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

    /// Whether a channel is one of lurker's rooms or the channels admins
    /// asked it to join.
    fn is_wanted(&self, channel: &str) -> bool {
        self.wanted()
            .iter()
            .any(|wanted| casemap::eq(wanted, channel))
    }

    /// Whether lurker stays in a channel for a nudge that is going.
    fn is_nudge_channel(&self, channel: &str) -> bool {
        log_db(self.db.nudge_channels())
            .unwrap_or_default()
            .iter()
            .any(|nudged| casemap::eq(nudged, channel))
    }

    /// The channels admins asked lurker to join, other than its rooms, that
    /// it is in right now.
    fn patrolled(&self) -> Vec<String> {
        log_db(self.db.channels())
            .unwrap_or_default()
            .into_iter()
            .filter(|channel| self.joined.contains(&casemap::fold(channel)))
            .collect()
    }

    fn room_of(&self, channel: &str) -> Option<Room> {
        self.settings.config.rooms.find(channel)
    }

    fn room_channel(&self, room: Room) -> &str {
        self.settings.config.rooms.channel(room)
    }

    /// Whether lurker has +o or higher in a channel.
    fn has_op(&self, channel: &str) -> bool {
        self.status
            .get(&casemap::fold(channel))
            .is_some_and(|status| status.iter().any(|&s| is_op(s)))
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
    use crate::config::Oper;

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

    /// Settings with an IRC operator login.
    fn oper_settings() -> Settings {
        let mut settings = Settings::example();
        settings.config.oper = Some(Oper {
            name: "lurker".to_owned(),
            password: "secret".to_owned(),
        });
        settings
    }

    /// A session in both rooms, logged in as an IRC operator.
    fn opered<'a>(settings: &'a mut Settings, db: &'a Db) -> Session<'a> {
        let mut session = in_both_rooms(settings, db, "lurker");
        let sent = feed(
            &mut session,
            &[":irc.example.net 381 lurker :You are now an IRC operator"],
        );
        // lurker ops itself in its rooms, and the server confirms it
        for room in ["#purgatory", "#FBI_watchlist"] {
            let samode =
                Command::SAMODE(room.to_owned(), "+o".to_owned(), Some("lurker".to_owned()));
            assert!(sent.contains(&samode), "{sent:?}");
            feed(
                &mut session,
                &[&format!(":irc.example.net MODE {room} +o lurker")],
            );
        }
        assert!(session.opered);
        session
    }

    /// The channels and nicks of the kicks lurker sends.
    fn kicks(sent: &[Command]) -> Vec<(&str, &str)> {
        sent.iter()
            .filter_map(|command| match command {
                Command::KICK(channel, nick, reason) => {
                    assert!(
                        reason.as_ref().is_some_and(|reason| !reason.is_empty()),
                        "a kick needs a reason"
                    );
                    Some((channel.as_str(), nick.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    /// The commands lurker sends, as short text without kick reasons, such
    /// as "KICK #wetfish bob".
    fn shapes(sent: &[Command]) -> Vec<String> {
        sent.iter()
            .map(|command| match command {
                Command::KICK(channel, nick, _) => format!("KICK {channel} {nick}"),
                Command::SAJOIN(nick, channel) => format!("SAJOIN {nick} {channel}"),
                Command::SAMODE(channel, mode, Some(nick)) => {
                    format!("SAMODE {channel} {mode} {nick}")
                }
                Command::PART(channel, None) => format!("PART {channel}"),
                Command::INVITE(nick, channel) => format!("INVITE {nick} {channel}"),
                Command::PRIVMSG(target, _) => format!("PRIVMSG {target}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn whois(nick: &str) -> Command {
        Command::WHOIS(None, nick.to_owned())
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
            &[&format!("{ADMIN} PRIVMSG lurker :trusted alice grace=3d")],
        );
        assert_eq!(
            sent,
            [reply(
                "alice is marked trusted (#purgatory). Kicked from every channel 3 days after \
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
            "PRIVMSG #purgatory :lurker: untrusted bob grace=1h",
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

        // Not from someone who isn't an admin, not without lurker's nick, and
        // not chat that happens to start with it
        let sent = feed(
            &mut session,
            &[
                ":mallory!~m@h PRIVMSG #purgatory :lurker: untrusted bob grace=1h",
                &format!("{ADMIN} PRIVMSG #purgatory :lurkers are untrusted"),
                &format!("{ADMIN} PRIVMSG #purgatory :untrusted bob grace=1h"),
                &format!(
                    "{ADMIN} PRIVMSG #purgatory :lurker keeps track of how long people are in \
                     purgatory"
                ),
                &format!("{ADMIN} PRIVMSG #purgatory :lurker: unchannel bob #wetfish"),
                &format!("{ADMIN} PRIVMSG #purgatory :lurker:"),
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn explains_unknown_commands_only_in_private() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(&mut session, &[&format!("{ADMIN} PRIVMSG lurker :keeps")]);
        let [reply] = sent.as_slice() else {
            panic!("expected one reply, got {sent:?}");
        };
        assert_eq!(said(reply).0, "rachel");
        assert!(said(reply).1.starts_with("I don't know the command"));
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
                ":rachel!~r@elsewhere.net PRIVMSG lurker :untrusted alice grace=1d",
                ":mallory!~m@home.example.net PRIVMSG lurker :untrusted alice grace=1d",
                &format!("{ADMIN} PRIVMSG #purgatory :untrusted alice grace=1d"),
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
        assert_eq!(sent, [join("#wetfish"), reply("Joining #wetfish.")]);
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
            [
                "My channels: #music (not opped). I'm not logged in as an IRC operator, so I \
                 don't warn or kick anyone."
            ]
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
            [
                "I'm only in my rooms. Invite me to a channel, or send: join <#channel>. I'm not \
                 logged in as an IRC operator, so I don't warn or kick anyone."
            ]
        );
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
    fn never_sends_the_same_welcome_twice_in_a_row() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");

        let mut previous = None;
        for n in 0..30 {
            let nick = format!("user{n}");
            mark(&db, &nick, Room::Purgatory, None);
            let sent = feed(&mut session, &[&format!(":{nick}!~l@h JOIN #purgatory")]);
            assert_eq!(sent.len(), 1, "{sent:?}");
            let index = session.last_welcome[&Room::Purgatory];
            assert_ne!(Some(index), previous, "welcome {index} twice in a row");
            previous = Some(index);
        }
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

    #[test]
    fn logs_in_as_an_irc_operator_after_connecting() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = Session::new(&mut settings, &db, messages());
        let sent = feed(&mut session, &[":irc.example.net 001 lurker :Welcome"]);
        assert_eq!(
            sent,
            [Command::OPER("lurker".to_owned(), "secret".to_owned())]
        );
        assert!(!session.opered);

        feed(
            &mut session,
            &[":irc.example.net 381 lurker :You are now an IRC operator"],
        );
        assert!(session.opered);
    }

    #[test]
    fn does_not_oper_without_a_login() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let mut session = Session::new(&mut settings, &db, messages());
        let sent = feed(&mut session, &[":irc.example.net 001 lurker :Welcome"]);
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[test]
    fn tells_admins_when_the_oper_login_fails() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        let sent = feed(
            &mut session,
            &[":irc.example.net 491 lurker :Invalid oper credentials"],
        );
        assert_eq!(
            sent,
            [reply(
                "I couldn't log in as an IRC operator (Invalid oper credentials), so I can't op \
                 myself, kick or sajoin lurkers. Check [oper] in lurker.toml and the oper block \
                 on the IRC server."
            )]
        );
        assert!(!session.opered);
    }

    #[test]
    fn does_nothing_to_lurkers_without_being_an_operator() {
        let mut settings = Settings::example();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = in_both_rooms(&mut settings, &db, "lurker");
        in_wetfish(&mut session, &db, "@", "bob");
        assert!(session.tick(DAY).is_empty());
    }

    #[test]
    fn ops_itself_in_its_channels_but_not_in_a_fight() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);
        db.add_channel("#wetfish", "rachel", 0).unwrap();

        let samode = Command::SAMODE(
            "#wetfish".to_owned(),
            "+o".to_owned(),
            Some("lurker".to_owned()),
        );
        let sent = feed(
            &mut session,
            &[
                ":lurker!lurker@host JOIN #wetfish",
                ":irc.example.net 353 lurker = #wetfish :+lurker carol",
                ":irc.example.net 366 lurker #wetfish :End of /NAMES list.",
            ],
        );
        assert_eq!(sent, std::slice::from_ref(&samode));

        feed(&mut session, &[":irc.example.net MODE #wetfish +o lurker"]);
        assert!(session.has_op("#wetfish"));
        assert!(session.op_myself(2000).is_empty());

        // Deopped again: lurker waits before trying again
        let sent = feed(&mut session, &[":op!op@h MODE #wetfish -o lurker"]);
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(session.op_myself(1000 + OP_RETRY), [samode]);
    }

    #[test]
    fn warns_lurkers_privately_even_in_its_channels() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        mark(&db, "carol", Room::Watchlist, None);
        let mut session = opered(&mut settings, &db);
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
        let (target, text) = said(warning);
        assert_eq!(target, "bob");
        assert!(text.contains("bob") && text.contains("1 HOUR"), "{text}");
        assert_eq!(db.lurker("bob").unwrap().unwrap().warned_at, Some(2000));
        assert_eq!(bob, db.lurker("bob").unwrap().unwrap().id);

        // Only once
        assert!(session.enforce(2100).is_empty());
    }

    #[test]
    fn warns_online_lurkers_elsewhere_privately() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        mark(&db, "bob", Room::Watchlist, Some(HOUR));
        let mut session = opered(&mut settings, &db);

        assert_eq!(session.tick(2000), [Command::ISON(vec!["bob".to_owned()])]);
        let sent = feed_at(&mut session, 2000, &[":irc.example.net 303 lurker :"]);
        assert!(sent.is_empty(), "offline: {sent:?}");

        session.tick(3000);
        let sent = feed_at(&mut session, 3000, &[":irc.example.net 303 lurker :Bob"]);
        let [warning] = sent.as_slice() else {
            panic!("expected a warning, got {sent:?}");
        };
        assert_eq!(said(warning).0, "bob");
        assert_eq!(db.lurker("bob").unwrap().unwrap().warned_at, Some(3000));

        // Nobody to ask about while bob's grace period runs
        assert!(session.tick(3000 + HOUR - 1).is_empty());
    }

    #[test]
    fn kicks_lurkers_without_a_warning_with_no_grace_period() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        mark(&db, "bob", Room::Watchlist, Some(0));
        let mut session = opered(&mut settings, &db);

        assert_eq!(session.tick(2000), [Command::ISON(vec!["bob".to_owned()])]);
        let sent = feed_at(&mut session, 2000, &[":irc.example.net 303 lurker :bob"]);
        assert_eq!(sent, [whois("bob")], "no warning, straight to the kicks");
        assert_eq!(db.lurker("bob").unwrap().unwrap().warned_at, Some(2000));

        let sent = feed_at(
            &mut session,
            2000,
            &[
                ":irc.example.net 311 lurker bob ~b host * :Bob",
                ":irc.example.net 319 lurker bob :#anime",
                ":irc.example.net 318 lurker bob :End of /WHOIS list.",
            ],
        );
        assert_eq!(kicks(&sent), [("#anime", "bob")]);
        assert!(
            sent.iter()
                .all(|command| !matches!(command, Command::PRIVMSG(..))),
            "{sent:?}"
        );
    }

    #[test]
    fn kicks_lurkers_from_every_channel_and_sajoins_them() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);

        session.tick(4000);
        let sent = feed_at(&mut session, 4000, &[":irc.example.net 303 lurker :bob"]);
        assert_eq!(sent, [whois("bob")]);

        let sent = feed_at(
            &mut session,
            4000,
            &[
                ":irc.example.net 311 lurker bob ~b host * :Bob",
                ":irc.example.net 319 lurker bob :@#wetfish #music",
                ":irc.example.net 319 lurker bob :&#secret",
                ":irc.example.net 318 lurker bob :End of /WHOIS list.",
            ],
        );
        assert_eq!(
            kicks(&sent),
            [("#wetfish", "bob"), ("#music", "bob"), ("#secret", "bob")]
        );
        assert_eq!(
            shapes(&sent[..4]),
            [
                "SAJOIN lurker #wetfish",
                "SAMODE #wetfish +o lurker",
                "KICK #wetfish bob",
                "PART #wetfish",
            ]
        );
        assert_eq!(
            sent.last(),
            Some(&Command::INVITE(
                "bob".to_owned(),
                "#FBI_watchlist".to_owned()
            ))
        );

        // Sajoined 30 to 60 seconds later, since bob didn't join
        assert!(session.invites_due(4029).is_empty());
        assert_eq!(
            session.invites_due(4060),
            [Command::SAJOIN(
                "bob".to_owned(),
                "#FBI_watchlist".to_owned()
            )]
        );
        assert!(session.invites_due(4120).is_empty(), "only once");

        // Looked up again a while later, for channels joined since
        assert!(session.enforce(4000 + SWEEP_INTERVAL - 1).is_empty());
        assert_eq!(session.enforce(4000 + SWEEP_INTERVAL), [whois("bob")]);
    }

    #[test]
    fn kicks_as_a_channel_op_wherever_the_lurker_is() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "bob");
        db.add_channel("#music", "rachel", 0).unwrap();
        db.add_channel("#art", "rachel", 0).unwrap();
        feed(&mut session, &[":lurker!lurker@host JOIN #music"]);

        let bob = db.lurker("bob").unwrap().unwrap();
        let channels =
            ["#wetfish", "#music", "#purgatory", "#elsewhere", "#art"].map(str::to_owned);
        let sent = session.sweep(&bob, &channels, 5000);
        assert_eq!(
            shapes(&sent),
            [
                // Opped already
                "KICK #wetfish bob",
                // One of its channels where it isn't opped: it keeps +o
                "SAMODE #music +o lurker",
                "KICK #music bob",
                // The other room, where it is opped
                "KICK #purgatory bob",
                // Not one of lurker's channels: in and out again
                "SAJOIN lurker #elsewhere",
                "SAMODE #elsewhere +o lurker",
                "KICK #elsewhere bob",
                "PART #elsewhere",
                // One of its channels that it isn't in yet
                "SAJOIN lurker #art",
                "SAMODE #art +o lurker",
                "KICK #art bob",
                "INVITE bob #FBI_watchlist",
            ]
        );
    }

    #[test]
    fn joins_a_channel_again_while_still_leaving_it() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);

        let first = session.kick("#elsewhere", "bob", "bye".to_owned(), 5000);
        assert_eq!(first.len(), 4, "{first:?}");
        // lurker sees itself join and get +o before it sees itself leave
        feed(
            &mut session,
            &[
                ":lurker!lurker@host JOIN #elsewhere",
                ":irc.example.net MODE #elsewhere +o lurker",
            ],
        );
        let second = session.kick("#elsewhere", "carol", "bye".to_owned(), 5000);
        assert_eq!(
            shapes(&second),
            [
                "SAJOIN lurker #elsewhere",
                "SAMODE #elsewhere +o lurker",
                "KICK #elsewhere carol",
                "PART #elsewhere",
            ]
        );

        // Still leaving after the first part, so a third kick joins again too
        feed(&mut session, &[":lurker!lurker@host PART #elsewhere"]);
        feed(&mut session, &[":lurker!lurker@host JOIN #elsewhere"]);
        let third = session.kick("#elsewhere", "dave", "bye".to_owned(), 5000);
        assert_eq!(shapes(&third)[0], "SAJOIN lurker #elsewhere");

        for _ in 0..2 {
            feed(&mut session, &[":lurker!lurker@host PART #elsewhere"]);
        }
        assert!(session.visiting.is_empty());
        assert!(session.rejoin().is_empty(), "it doesn't come back");
    }

    #[test]
    fn does_not_sajoin_lurkers_who_join_on_the_invite() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);

        let lurker = db.lurker("bob").unwrap().unwrap();
        let sent = session.sweep(&lurker, &["#anime".to_owned()], 4000);
        assert_eq!(
            sent.last(),
            Some(&Command::INVITE(
                "bob".to_owned(),
                "#FBI_watchlist".to_owned()
            ))
        );
        // Not invited twice while the first invite is waiting
        session.swept.clear();
        let again = session.sweep(&lurker, &[], 4010);
        assert!(again.is_empty(), "{again:?}");

        feed_at(&mut session, 4020, &[":bob!~b@h JOIN #FBI_watchlist"]);
        assert!(session.invited.is_empty());
        assert!(session.invites_due(4060).is_empty());
    }

    #[test]
    fn does_not_sajoin_lurkers_who_spoke_while_invited() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);

        let lurker = db.lurker("bob").unwrap().unwrap();
        session.sweep(&lurker, &[], 4000);
        db.set_spoke(bob, 4010).unwrap();
        assert!(session.invites_due(4060).is_empty());
        assert!(session.invited.is_empty());
    }

    #[test]
    fn invites_lurkers_sajoined_before_lurker_sent_invites() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        feed(
            &mut session,
            &[":irc.example.net 353 lurker = #FBI_watchlist :@lurker bob"],
        );

        let lurker = db.lurker("bob").unwrap().unwrap();
        assert_eq!(lurker.invited_at, None);
        let sent = session.sweep(&lurker, &["#FBI_watchlist".to_owned()], 4000);
        assert_eq!(shapes(&sent), ["KICK #FBI_watchlist bob"]);
        assert!(!session.presence.contains("#FBI_watchlist", "bob"));

        // Invited a minute later, once the bridge has passed the kick on
        assert!(session.sweep(&lurker, &[], 4010).is_empty(), "not yet");
        assert!(session.invites_due(4059).is_empty());
        assert_eq!(
            shapes(&session.invites_due(4060)),
            ["INVITE bob #FBI_watchlist"]
        );
        assert_eq!(db.lurker("bob").unwrap().unwrap().invited_at, Some(4060));

        // Once invited, they are left in the room
        session.invited.clear();
        let lurker = db.lurker("bob").unwrap().unwrap();
        feed(&mut session, &[":bob!~b@h JOIN #FBI_watchlist"]);
        let sent = session.sweep(&lurker, &["#FBI_watchlist".to_owned()], 5000);
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[test]
    fn ops_itself_to_invite_where_it_isnt_opped() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        feed(&mut session, &[":op!op@h MODE #FBI_watchlist -o lurker"]);

        let lurker = db.lurker("bob").unwrap().unwrap();
        let sent = session.sweep(&lurker, &[], 4000);
        assert_eq!(
            shapes(&sent),
            [
                "SAMODE #FBI_watchlist +o lurker",
                "INVITE bob #FBI_watchlist"
            ]
        );

        // Kicking from the room and inviting back asks for +o only once
        session.invited.clear();
        session.op_asked.clear();
        feed(
            &mut session,
            &[":irc.example.net 353 lurker = #FBI_watchlist :lurker bob"],
        );
        db.set_invited(bob, 0).unwrap();
        let mut lurker = db.lurker("bob").unwrap().unwrap();
        lurker.invited_at = None;
        let sent = session.sweep(&lurker, &["#FBI_watchlist".to_owned()], 5000);
        assert_eq!(
            shapes(&sent),
            ["SAMODE #FBI_watchlist +o lurker", "KICK #FBI_watchlist bob"]
        );
        feed(
            &mut session,
            &[":irc.example.net MODE #FBI_watchlist +o lurker"],
        );
        assert_eq!(
            shapes(&session.invites_due(5060)),
            ["INVITE bob #FBI_watchlist"]
        );
    }

    #[test]
    fn does_not_invite_lurkers_back_in_their_room_after_the_kick() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        feed(
            &mut session,
            &[":irc.example.net 353 lurker = #FBI_watchlist :@lurker bob"],
        );

        let lurker = db.lurker("bob").unwrap().unwrap();
        session.sweep(&lurker, &["#FBI_watchlist".to_owned()], 4000);
        feed(&mut session, &[":bob!~b@h JOIN #FBI_watchlist"]);
        assert!(session.invites_due(4060).is_empty());
        // Recorded, so they aren't kicked from the room again
        assert_eq!(db.lurker("bob").unwrap().unwrap().invited_at, Some(4060));
    }

    #[test]
    fn leaves_lurkers_in_their_own_room() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        db.set_invited(bob, 100).unwrap();
        let mut session = opered(&mut settings, &db);
        feed(
            &mut session,
            &[":irc.example.net 353 lurker = #FBI_watchlist :@lurker bob"],
        );

        let sent = session.enforce(4000);
        assert_eq!(sent, [whois("bob")]);
        let sent = feed_at(
            &mut session,
            4000,
            &[
                ":irc.example.net 311 lurker bob ~b host * :Bob",
                ":irc.example.net 319 lurker bob :#FBI_watchlist #wetfish",
                ":irc.example.net 318 lurker bob :End of /WHOIS list.",
            ],
        );
        assert_eq!(kicks(&sent), [("#wetfish", "bob")]);
        assert!(
            !sent
                .iter()
                .any(|command| matches!(command, Command::INVITE(..))),
            "no invite: {sent:?}"
        );
        assert!(session.invited.is_empty());
    }

    #[test]
    fn kicks_lurkers_who_join_its_channels_straight_away() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "");
        session.swept.insert(bob, 4000);

        let sent = feed_at(&mut session, 4100, &[":bob!~b@h JOIN #wetfish"]);
        assert_eq!(sent, [whois("bob")]);

        // Joining their own room is fine
        session.lookups.clear();
        session.swept.insert(bob, 4100);
        let sent = feed_at(&mut session, 4200, &[":bob!~b@h JOIN #FBI_watchlist"]);
        assert!(
            kicks(&sent).is_empty() && !sent.contains(&whois("bob")),
            "{sent:?}"
        );
    }

    #[test]
    fn leaves_offline_lurkers_alone() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);

        session.tick(4000);
        feed_at(&mut session, 4000, &[":irc.example.net 303 lurker :bob"]);
        let sent = feed_at(
            &mut session,
            4000,
            &[
                ":irc.example.net 401 lurker bob :No such nick",
                ":irc.example.net 318 lurker bob :End of /WHOIS list.",
            ],
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert!(!session.is_online("bob"));
    }

    #[test]
    fn speaking_after_the_warning_stops_the_kicks() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let bob = mark(&db, "bob", Room::Watchlist, Some(HOUR));
        db.set_warned(bob, 0).unwrap();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "bob");
        session.lookups.clear();

        feed_at(
            &mut session,
            100,
            &[":bob!~b@h PRIVMSG #wetfish :hi, I'm real"],
        );
        assert!(db.lurker("bob").unwrap().unwrap().released());
        assert!(session.enforce(DAY).is_empty());

        // Marking them again with a grace period starts over with a warning
        let sent = feed_at(
            &mut session,
            DAY,
            &[&format!("{ADMIN} PRIVMSG lurker :untrusted bob grace=1h")],
        );
        let (target, text) = said(&sent[0]);
        assert_eq!(target, "bob");
        assert!(text.contains("1 HOUR"), "{text}");
        assert_eq!(said(&sent[1]).0, "rachel");
    }

    #[test]
    fn leaves_lurkers_without_a_grace_period_alone() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        mark(&db, "carol", Room::Purgatory, None);
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "carol");
        assert!(session.tick(DAY).is_empty());
    }

    #[test]
    fn reports_its_operator_status() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "");
        assert_eq!(
            session.channels_report(),
            "My channels: #wetfish (opped). I'm logged in as an IRC operator, so I warn and \
             kick lurkers."
        );
    }

    #[test]
    fn reminds_nudged_people_everywhere_until_they_speak() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "");
        db.add_nudge("alice", 3 * DAY, 3, "rachel", 1000).unwrap();

        assert_eq!(
            session.tick(1000),
            [Command::ISON(vec!["alice".to_owned()])]
        );
        let sent = feed_at(&mut session, 1000, &[":irc.example.net 303 lurker :alice"]);
        assert_eq!(sent, [whois("alice")]);
        let sent = feed_at(
            &mut session,
            1000,
            &[
                ":irc.example.net 311 lurker alice ~a host * :Alice",
                ":irc.example.net 319 lurker alice :#wetfish @#anime",
                ":irc.example.net 318 lurker alice :End of /WHOIS list.",
            ],
        );
        assert_eq!(
            shapes(&sent),
            ["PRIVMSG #wetfish", "SAJOIN lurker #anime", "PRIVMSG #anime"]
        );
        for command in &sent {
            if let Command::PRIVMSG(_, text) = command {
                assert!(text.starts_with("alice: "), "{text}");
            }
        }
        let alice = db.nudge("alice").unwrap().unwrap();
        assert_eq!((alice.sent, alice.first_reminder_at), (1, Some(1000)));
        assert_eq!(db.nudge_channels().unwrap(), ["#anime", "#wetfish"]);

        // Nothing more until the next reminder is due, a day later
        assert!(session.tick(1000 + HOUR).is_empty());

        // lurker stays in #anime, and hears alice answer there
        feed(&mut session, &[":lurker!lurker@host JOIN #anime"]);
        assert!(session.nudge_joined.contains_key("#anime"));
        let sent = feed_at(
            &mut session,
            1000 + 2 * HOUR,
            &[":alice!~a@h PRIVMSG #anime :oh hi"],
        );
        let [notice] = sent.as_slice() else {
            panic!("expected a notice, got {sent:?}");
        };
        assert_eq!(
            said(notice),
            (
                "rachel",
                "Nudge: alice spoke in #anime 2 hours after the first reminder, with 1 of 3 \
                 reminders sent, so the reminders stopped: \"oh hi\""
            )
        );
        assert_eq!(db.nudge("alice").unwrap(), None);

        // Then leaves #anime, but not #wetfish, which an admin asked for
        assert_eq!(
            session.tick(1000 + 2 * HOUR),
            [Command::PART("#anime".to_owned(), None)]
        );
    }

    #[test]
    fn waits_to_remind_in_a_channel_just_reminded() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);
        in_wetfish(&mut session, &db, "@", "");
        db.add_nudge("alice", 3 * DAY, 3, "rachel", 1000).unwrap();
        db.add_nudge("carol", 3 * DAY, 3, "rachel", 1000).unwrap();
        let alice = db.nudge("alice").unwrap().unwrap();
        let carol = db.nudge("carol").unwrap().unwrap();
        let channels = ["#wetfish".to_owned()];

        assert_eq!(
            shapes(&session.remind(&alice, &channels, 1000)),
            ["PRIVMSG #wetfish"]
        );
        assert!(session.remind(&carol, &channels, 1060).is_empty());
        assert_eq!(db.nudge("carol").unwrap().unwrap().sent, 0);
        assert_eq!(
            shapes(&session.remind(&carol, &channels, 1000 + schedule::ROOM_GAP)),
            ["PRIVMSG #wetfish"]
        );
    }

    #[test]
    fn tells_admins_when_a_nudge_is_over() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        let mut session = opered(&mut settings, &db);
        db.add_nudge("bob", 2 * HOUR, 2, "rachel", 0).unwrap();

        assert!(session.end_nudges(2 * HOUR - 1).is_empty());
        assert_eq!(
            session.end_nudges(2 * HOUR),
            [Command::PRIVMSG(
                "rachel".to_owned(),
                "Nudge: bob's nudge is over without a reply. They weren't online for any of its \
                 2 reminders. Over to you to review; nudge list has the rest."
                    .to_owned()
            )]
        );
        assert!(session.end_nudges(3 * HOUR).is_empty());
        assert_eq!(db.nudges().unwrap()[0].ended, Some(NudgeEnd::Finished));
    }

    #[test]
    fn rejoins_nudge_channels_after_a_restart() {
        let mut settings = oper_settings();
        let db = Db::open_in_memory();
        db.add_nudge("alice", 3 * DAY, 3, "rachel", 0).unwrap();
        let id = db.nudge("alice").unwrap().unwrap().id;
        db.add_nudge_channel(id, "#anime").unwrap();

        let session = opered(&mut settings, &db);
        assert_eq!(
            session.rejoin(),
            [Command::SAJOIN("lurker".to_owned(), "#anime".to_owned())]
        );
    }

    #[test]
    fn reads_channels_from_whois_replies() {
        let channels: Vec<&str> = whois_channels("@#a %+#b #c ~&#d &#e").collect();
        assert_eq!(channels, ["#a", "#b", "#c", "#d", "#e"]);
    }
}
