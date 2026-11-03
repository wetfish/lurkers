//! Keeps lurker on IRC: connects over TLS, joins both rooms and rejoins them
//! when needed, takes its nick back when it was taken, reconnects after a
//! lost connection, and quits with a message when lurker is stopped. Along
//! the way it passes what happens in the rooms on to `presence`.

use std::collections::HashSet;
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use irc::client::ClientStream;
use irc::client::prelude::{
    Capability, Client, Command, Config as IrcConfig, Message, Prefix, Response,
};
use irc::proto::CapSubCommand;
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::casemap;
use crate::config::{Config, Room};
use crate::db::{Db, Key};
use crate::presence::{Account, Arrival, Presence, WHOX_TOKEN};
use crate::shutdown::Shutdown;

/// Sent with QUIT when lurker is stopped, which is usually for a deploy.
pub const QUIT_MESSAGE: &str = "brb, getting a software update";

/// The IRCv3 capabilities lurker asks for, each on its own so a server
/// lacking one still enables the others. extended-join and account-notify
/// report services accounts, and chghost reports host changes as such
/// rather than as a quit and a join.
const CAPABILITIES: [Capability; 3] = [
    Capability::ExtendedJoin,
    Capability::AccountNotify,
    Capability::ChgHost,
];

/// How long connecting, including the TLS handshake, may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the server to close the connection after QUIT.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to join the rooms lurker is not in, such as after a kick.
const REJOIN_INTERVAL: Duration = Duration::from_secs(60);

/// The wait before reconnecting, doubled after each attempt up to
/// `MAX_RETRY_DELAY`.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// A connection that lasted this long counts as having worked, so the wait
/// before reconnecting starts again from `FIRST_RETRY_DELAY`.
const STABLE_CONNECTION: Duration = Duration::from_secs(300);

/// Stays connected, reconnecting whenever the connection is lost, until
/// lurker is stopped.
pub async fn run(config: &Config, db: &Db, shutdown: &mut Shutdown) {
    let mut delay = FIRST_RETRY_DELAY;

    loop {
        let started = Instant::now();

        match connect(config, db, shutdown).await {
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
async fn connect(config: &Config, db: &Db, shutdown: &mut Shutdown) -> Result<(), String> {
    let server = &config.irc.server;
    eprintln!("Connecting to {server}:{}", config.irc.port);

    let connecting = time::timeout(CONNECT_TIMEOUT, Client::from_config(irc_config(config)));
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

    for capability in CAPABILITIES {
        client
            .send_cap_req(&[capability])
            .map_err(|err| describe(&err))?;
    }
    client.identify().map_err(|err| describe(&err))?;
    let mut stream = client.stream().map_err(|err| describe(&err))?;

    let mut session = Session::new(config, db);
    let result = serve(&client, &mut stream, &mut session, shutdown).await;
    session.close(unix_now());
    result
}

/// Handles messages and rejoins rooms until the connection is lost or
/// lurker is stopped.
async fn serve(
    client: &Client,
    stream: &mut ClientStream,
    session: &mut Session<'_>,
    shutdown: &mut Shutdown,
) -> Result<(), String> {
    let mut rejoin = time::interval(REJOIN_INTERVAL);
    rejoin.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let commands = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(message)) => session.handle(&message, unix_now()),
                Some(Err(err)) => return Err(format!("connection lost: {}", describe(&err))),
                None => return Err("the server closed the connection".to_owned()),
            },
            _ = rejoin.tick() => session.rejoin(),
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
fn irc_config(config: &Config) -> IrcConfig {
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

/// Logs a database error, so a problem with one record doesn't drop the
/// connection.
fn log_db<T>(result: rusqlite::Result<T>) -> Option<T> {
    result
        .map_err(|err| eprintln!("error: database: {err}"))
        .ok()
}

/// What lurker knows about its current connection.
struct Session<'a> {
    config: &'a Config,
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
    fn new(config: &'a Config, db: &'a Db) -> Session<'a> {
        Session {
            config,
            db,
            nick: config.irc.nick.clone(),
            registered: false,
            joined: HashSet::new(),
            presence: Presence::default(),
        }
    }

    /// Updates what lurker knows from a message, and returns the commands
    /// to send in reply.
    fn handle(&mut self, message: &Message, now: i64) -> Vec<Command> {
        let config = self.config;
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
            Command::Response(Response::RPL_ISUPPORT, args) => self.presence.isupport(args),
            Command::CAP(_, subcommand, first, second) => {
                let capabilities = second.as_deref().or(first.as_deref()).unwrap_or("");
                match subcommand {
                    CapSubCommand::ACK => eprintln!("Server enabled {capabilities}"),
                    CapSubCommand::NAK => eprintln!("Server does not support {capabilities}"),
                    _ => {}
                }
            }

            // Who was already in a room when lurker joined it
            Command::Response(Response::RPL_WHOREPLY, args) => {
                // <me> <channel> <ident> <host> <server> <nick> <flags> :<hops> <real name>
                if let [_, channel, ident, host, _, nick, ..] = args.as_slice() {
                    self.listed(channel, Key::new(nick, ident, host), Account::Unknown, now);
                }
            }
            Command::Raw(code, args) if code == "354" => {
                // <me> <token> <channel> <ident> <host> <nick> <flags> <account>
                if let [_, token, channel, ident, host, nick, _, account] = args.as_slice()
                    && token == WHOX_TOKEN
                {
                    let account = Account::parse(account);
                    self.listed(channel, Key::new(nick, ident, host), account, now);
                }
            }
            Command::Response(Response::RPL_ENDOFWHO, args) => {
                if let Some(room) = args.get(1).and_then(|channel| config.rooms.find(channel)) {
                    eprintln!(
                        "{}: {} people besides lurker",
                        config.rooms.channel(room),
                        self.presence.count(room)
                    );
                }
            }

            Command::JOIN(channels, account, real_name) => {
                // With extended-join, JOIN also gives the account and real name
                let account = match (account, real_name) {
                    (Some(account), Some(_)) => Account::parse(account),
                    _ => Account::Unknown,
                };
                for (room, channel) in rooms_in(config, channels) {
                    if self.is_me(source) {
                        self.joined.insert(room);
                        eprintln!("Joined {channel}");
                        commands.push(self.presence.who(channel));
                    } else if let Some(key) = source_key(message) {
                        self.arrived(room, key, account, true, now);
                    }
                }
            }
            Command::PART(channels, _) => {
                for (room, channel) in rooms_in(config, channels) {
                    if self.is_me(source) {
                        self.lost_room(room, now);
                        eprintln!("Left {channel}");
                    } else {
                        self.departed(room, source, "left", now);
                    }
                }
            }
            Command::KICK(channels, target, reason) => {
                for (room, channel) in rooms_in(config, channels) {
                    if self.is_me(target) {
                        self.lost_room(room, now);
                        eprintln!(
                            "Kicked from {channel} by {source} ({}), rejoining within a minute",
                            reason.as_deref().unwrap_or("no reason")
                        );
                    } else {
                        self.departed(room, target, "was kicked from", now);
                    }
                }
            }
            Command::QUIT(_) => {
                if let Some(key) = log_db(self.presence.quit(self.db, source, now)).flatten() {
                    eprintln!("{key} quit");
                }
                commands.extend(self.reclaim_nick(source));
            }
            Command::NICK(new_nick) if self.is_me(source) => {
                eprintln!("Now known as {new_nick}");
                self.nick = new_nick.clone();
            }
            Command::NICK(new_nick) => {
                let renamed = self.presence.renamed(self.db, source, new_nick, now);
                if let Some(key) = log_db(renamed).flatten() {
                    eprintln!("{source} is now {key}");
                }
                commands.extend(self.reclaim_nick(source));
            }
            Command::CHGHOST(ident, host) => {
                let changed = self
                    .presence
                    .host_changed(self.db, source, ident, host, now);
                if let Some(key) = log_db(changed).flatten() {
                    eprintln!("{source} is now {key}");
                }
            }
            Command::ACCOUNT(account) => {
                let account = Account::parse(account);
                log_db(self.presence.account_changed(self.db, source, account));
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
            .map(|room| Command::JOIN(self.config.rooms.channel(room).to_owned(), None, None))
            .collect()
    }

    /// Records the last time everyone was seen in the rooms, since the
    /// connection has ended.
    fn close(&mut self, now: i64) {
        for room in Room::ALL {
            self.lost_room(room, now);
        }
    }

    /// Someone a WHO reply listed in a channel.
    fn listed(&mut self, channel: &str, key: Key, account: Account, now: i64) {
        if let Some(room) = self.config.rooms.find(channel) {
            self.arrived(room, key, account, false, now);
        }
    }

    /// Someone is in a room. Joins are logged; people a WHO reply lists are
    /// only logged when they are new, since a reconnect lists everyone.
    fn arrived(&mut self, room: Room, key: Key, account: Account, joined: bool, now: i64) {
        if self.is_lurker(&key.nick) {
            return;
        }

        let channel = self.config.rooms.channel(room);
        let arrival = self
            .presence
            .arrived(self.db, room, key.clone(), account, now);
        match log_db(arrival) {
            Some(Arrival::First) => eprintln!("{key} is new in {channel}"),
            Some(Arrival::Returned) if joined => eprintln!("{key} is back in {channel}"),
            _ => {}
        }
    }

    fn departed(&mut self, room: Room, nick: &str, how: &str, now: i64) {
        let channel = self.config.rooms.channel(room);
        if let Some(key) = log_db(self.presence.departed(self.db, room, nick, now)).flatten() {
            eprintln!("{key} {how} {channel}");
        }
    }

    fn lost_room(&mut self, room: Room, now: i64) {
        self.joined.remove(&room);
        log_db(self.presence.lost_room(self.db, room, now));
    }

    /// Asks for lurker's nick back when whoever had it quit or changed nick.
    fn reclaim_nick(&self, source: &str) -> Option<Command> {
        let nick = &self.config.irc.nick;
        if !casemap::eq(source, nick) || !self.registered || casemap::eq(&self.nick, nick) {
            return None;
        }
        eprintln!("Taking back the nick {nick}");
        Some(Command::NICK(nick.clone()))
    }

    fn is_me(&self, nick: &str) -> bool {
        casemap::eq(nick, &self.nick)
    }

    /// Whether a nick is lurker, or one of the nicks it uses, such as an old
    /// connection of its own that the server hasn't dropped yet.
    fn is_lurker(&self, nick: &str) -> bool {
        let own = &self.config.irc.nick;
        self.is_me(nick)
            || casemap::eq(nick, own)
            || alt_nicks(own).iter().any(|alt| casemap::eq(nick, alt))
    }
}

/// The channels in a comma-separated list that are lurker's rooms.
fn rooms_in<'c>(config: &'c Config, channels: &'c str) -> impl Iterator<Item = (Room, &'c str)> {
    channels
        .split(',')
        .filter_map(move |channel| config.rooms.find(channel).map(|room| (room, channel)))
}

/// The nick and `ident@host` a message came from.
fn source_key(message: &Message) -> Option<Key> {
    match &message.prefix {
        Some(Prefix::Nickname(nick, ident, host)) if !ident.is_empty() && !host.is_empty() => {
            Some(Key::new(nick, ident, host))
        }
        _ => None,
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

    fn who(room: &str) -> Command {
        Command::WHO(Some(room.to_owned()), None)
    }

    /// Feeds raw lines to a session, returning what it sends back.
    fn feed(session: &mut Session<'_>, lines: &[&str]) -> Vec<Command> {
        lines
            .iter()
            .flat_map(|line| session.handle(&message(line), 1000))
            .collect()
    }

    /// A session that has connected as `nick` and joined both rooms.
    fn in_both_rooms<'a>(config: &'a Config, db: &'a Db, nick: &str) -> Session<'a> {
        let mut session = Session::new(config, db);
        let lines = [
            format!(":irc.example.net 001 {nick} :Welcome"),
            format!(":{nick}!lurker@host JOIN #purgatory"),
            format!(":{nick}!lurker@host JOIN :#FBI_watchlist"),
        ];
        feed(&mut session, &lines.each_ref().map(String::as_str));
        session
    }

    #[test]
    fn waits_for_the_welcome_before_rejoining() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = Session::new(&config, &db);
        assert!(session.rejoin().is_empty());

        feed(&mut session, &[":irc.example.net 001 lurker :Welcome"]);
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist")]
        );
    }

    #[test]
    fn lists_who_is_in_each_room_it_joins() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = Session::new(&config, &db);
        let sent = feed(
            &mut session,
            &[
                ":irc.example.net 001 lurker :Welcome",
                ":lurker!lurker@host JOIN #purgatory",
                ":Lurker!lurker@host JOIN #FBI_Watchlist",
            ],
        );
        assert_eq!(sent, [who("#purgatory"), who("#FBI_Watchlist")]);
        assert!(session.rejoin().is_empty());
    }

    #[test]
    fn uses_whox_when_the_server_supports_it() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = Session::new(&config, &db);
        let sent = feed(
            &mut session,
            &[
                ":irc.example.net 005 lurker WHOX CHANTYPES=# :are supported by this server",
                ":irc.example.net 001 lurker :Welcome",
                ":lurker!lurker@host JOIN #purgatory",
                ":irc.example.net 354 lurker 152 #purgatory ~a fish-1.chat alice H alice",
                ":irc.example.net 354 lurker 152 #purgatory ~b fish-2.chat bob H 0",
                ":irc.example.net 354 lurker 152 #purgatory lurker host lurker H 0",
                ":irc.example.net 315 lurker #purgatory :End of /WHO list.",
            ],
        );
        assert_eq!(
            sent,
            [Command::Raw(
                "WHO".to_owned(),
                vec!["#purgatory".to_owned(), "%tcuhnfa,152".to_owned()]
            )]
        );

        assert_eq!(session.presence.count(Room::Purgatory), 2);
        assert_eq!(db.user_count(), 2);
        let alice = session.presence.user_in(Room::Purgatory, "alice").unwrap();
        assert_eq!(db.user(alice).1.as_deref(), Some("alice"));
        let bob = session.presence.user_in(Room::Purgatory, "bob").unwrap();
        assert_eq!(db.user(bob).1, None);
    }

    #[test]
    fn records_people_from_plain_who_replies() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(
            &mut session,
            &[
                ":irc.example.net 352 lurker #FBI_watchlist ~a fish-1.chat irc.example.net alice H :0 Alice",
                ":irc.example.net 352 lurker #wetfish ~c fish-3.chat irc.example.net carol H :0 Carol",
            ],
        );
        assert_eq!(session.presence.count(Room::Watchlist), 1);
        assert_eq!(db.user_count(), 1);
    }

    #[test]
    fn follows_joins_parts_kicks_and_quits() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(
            &mut session,
            &[
                ":alice!~a@fish-1.chat JOIN #purgatory",
                ":alice!~a@fish-1.chat JOIN #FBI_watchlist",
                ":bob!~b@fish-2.chat JOIN #purgatory",
                ":carol!~c@fish-3.chat JOIN #wetfish",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 2);
        assert_eq!(session.presence.count(Room::Watchlist), 1);
        assert_eq!(db.user_count(), 2);

        feed(
            &mut session,
            &[
                ":bob!~b@fish-2.chat PART #purgatory :bye",
                ":op!op@host KICK #FBI_watchlist alice :out",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 1);
        assert_eq!(session.presence.count(Room::Watchlist), 0);

        feed(&mut session, &[":alice!~a@fish-1.chat QUIT :Ping timeout"]);
        assert_eq!(session.presence.count(Room::Purgatory), 0);
    }

    #[test]
    fn reads_accounts_from_extended_joins_and_account_changes() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory alice :Alice"]);
        let alice = session.presence.user_in(Room::Purgatory, "alice").unwrap();
        assert_eq!(db.user(alice).1.as_deref(), Some("alice"));

        feed(&mut session, &[":alice!~a@h ACCOUNT *"]);
        assert_eq!(db.user(alice).1, None);

        feed(&mut session, &[":bob!~b@h JOIN #purgatory * :Bob"]);
        let bob = session.presence.user_in(Room::Purgatory, "bob").unwrap();
        assert_eq!(db.user(bob).1, None);
    }

    #[test]
    fn follows_nick_and_host_changes() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(&mut session, &[":alice!~a@old JOIN #purgatory"]);
        let alice = session.presence.user_in(Room::Purgatory, "alice").unwrap();

        feed(
            &mut session,
            &[":alice!~a@old NICK alice_", ":alice_!~a@old CHGHOST ~a new"],
        );
        assert_eq!(
            session.presence.user_in(Room::Purgatory, "alice_"),
            Some(alice)
        );
        assert_eq!(
            db.find_user(&Key::new("alice_", "~a", "new")).unwrap(),
            Some(alice)
        );
    }

    #[test]
    fn does_not_track_itself_or_its_old_connection() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker_");
        feed(
            &mut session,
            &[
                ":irc.example.net 352 lurker_ #purgatory lurker host irc.example.net lurker_ H :0 lurker",
                ":irc.example.net 352 lurker_ #purgatory lurker old irc.example.net lurker H :0 lurker",
            ],
        );
        assert_eq!(session.presence.count(Room::Purgatory), 0);
        assert_eq!(db.user_count(), 0);
    }

    #[test]
    fn rejoins_and_relists_after_a_kick() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);

        feed(&mut session, &[":op!op@host KICK #purgatory lurker :out"]);
        assert_eq!(session.presence.count(Room::Purgatory), 0);
        assert_eq!(session.rejoin(), [join("#purgatory")]);

        let sent = feed(&mut session, &[":lurker!lurker@host JOIN #purgatory"]);
        assert_eq!(sent, [who("#purgatory")]);
        assert!(session.rejoin().is_empty());
    }

    #[test]
    fn rejoins_after_parting() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(&mut session, &[":lurker!lurker@host PART #FBI_watchlist"]);
        assert_eq!(session.rejoin(), [join("#FBI_watchlist")]);
    }

    #[test]
    fn follows_its_own_nick_changes() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker_");

        let sent = feed(&mut session, &[":lurker_!lurker@host NICK lurker"]);
        assert!(sent.is_empty());
        assert_eq!(session.nick, "lurker");

        feed(&mut session, &[":op!op@host KICK #purgatory lurker :out"]);
        assert_eq!(session.rejoin(), [join("#purgatory")]);
    }

    #[test]
    fn takes_its_nick_back_when_it_is_freed() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker_");

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
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        let sent = feed(&mut session, &[":alice!alice@host QUIT :bye"]);
        assert!(sent.is_empty());
    }

    #[test]
    fn closing_records_when_everyone_was_last_seen() {
        let config = Config::example();
        let db = Db::open_in_memory();
        let mut session = in_both_rooms(&config, &db, "lurker");
        feed(&mut session, &[":alice!~a@h JOIN #purgatory"]);
        session.close(2000);
        assert_eq!(session.presence.count(Room::Purgatory), 0);
        assert_eq!(session.rejoin().len(), 2);
    }
}
