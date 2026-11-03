//! Keeps lurker on IRC: connects over TLS, joins both rooms and rejoins them
//! when needed, takes its nick back when it was taken, reconnects after a
//! lost connection, and quits with a message when lurker is stopped.

use std::collections::HashSet;
use std::error::Error;
use std::time::Duration;

use futures_util::StreamExt;
use irc::client::ClientStream;
use irc::client::prelude::{Client, Command, Config as IrcConfig, Message, Response};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::casemap;
use crate::config::Config;
use crate::shutdown::Shutdown;

/// Sent with QUIT when lurker is stopped, which is usually for a deploy.
pub const QUIT_MESSAGE: &str = "brb, getting a software update";

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
pub async fn run(config: &Config, shutdown: &mut Shutdown) {
    let mut delay = FIRST_RETRY_DELAY;

    loop {
        let started = Instant::now();

        match connect(config, shutdown).await {
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
async fn connect(config: &Config, shutdown: &mut Shutdown) -> Result<(), String> {
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

    client.identify().map_err(|err| describe(&err))?;
    let mut stream = client.stream().map_err(|err| describe(&err))?;
    let mut session = Session::new(config);
    let mut rejoin = time::interval(REJOIN_INTERVAL);
    rejoin.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let commands = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(message)) => session.handle(&message),
                Some(Err(err)) => return Err(format!("connection lost: {}", describe(&err))),
                None => return Err("the server closed the connection".to_owned()),
            },
            _ = rejoin.tick() => session.rejoin(),
            () = shutdown.recv() => {
                quit(&client, &mut stream).await;
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
/// tries `<nick>_` and `<nick>__` when the nick is taken.
fn irc_config(config: &Config) -> IrcConfig {
    let nick = &config.irc.nick;

    IrcConfig {
        nickname: Some(nick.clone()),
        alt_nicks: vec![format!("{nick}_"), format!("{nick}__")],
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

/// What lurker knows about its current connection.
struct Session<'a> {
    config: &'a Config,
    /// lurker's nick on this connection: the configured one, or `<nick>_`
    /// when that was taken
    nick: String,
    /// Whether the server has accepted the connection
    registered: bool,
    /// The rooms lurker is in, folded with `casemap::fold`
    joined: HashSet<String>,
}

impl<'a> Session<'a> {
    fn new(config: &'a Config) -> Session<'a> {
        Session {
            config,
            nick: config.irc.nick.clone(),
            registered: false,
            joined: HashSet::new(),
        }
    }

    /// Updates what lurker knows from a message, and returns the commands
    /// to send in reply.
    fn handle(&mut self, message: &Message) -> Vec<Command> {
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
            Command::JOIN(channels, _, _) if self.is_me(source) => {
                for room in rooms_in(config, channels) {
                    self.joined.insert(casemap::fold(room));
                    eprintln!("Joined {room}");
                }
            }
            Command::PART(channels, _) if self.is_me(source) => {
                for room in rooms_in(config, channels) {
                    self.joined.remove(&casemap::fold(room));
                    eprintln!("Left {room}");
                }
            }
            Command::KICK(channels, target, reason) if self.is_me(target) => {
                for room in rooms_in(config, channels) {
                    self.joined.remove(&casemap::fold(room));
                    eprintln!(
                        "Kicked from {room} by {source} ({}), rejoining within a minute",
                        reason.as_deref().unwrap_or("no reason")
                    );
                }
            }
            Command::NICK(new_nick) if self.is_me(source) => {
                eprintln!("Now known as {new_nick}");
                self.nick = new_nick.clone();
            }
            // Whoever had lurker's nick quit or changed nick, so it is free
            Command::NICK(_) | Command::QUIT(_) if casemap::eq(source, &config.irc.nick) => {
                if self.registered && !casemap::eq(&self.nick, &config.irc.nick) {
                    eprintln!("Taking back the nick {}", config.irc.nick);
                    commands.push(Command::NICK(config.irc.nick.clone()));
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

        self.config
            .rooms
            .all()
            .into_iter()
            .filter(|room| !self.joined.contains(&casemap::fold(room)))
            .map(|room| Command::JOIN(room.to_owned(), None, None))
            .collect()
    }

    fn is_me(&self, nick: &str) -> bool {
        casemap::eq(nick, &self.nick)
    }
}

/// The channels in a comma-separated list that are lurker's rooms.
fn rooms_in<'c>(config: &Config, channels: &'c str) -> impl Iterator<Item = &'c str> {
    let rooms = config.rooms.all().map(casemap::fold);
    channels
        .split(',')
        .filter(move |channel| rooms.contains(&casemap::fold(channel)))
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

    /// A session that has connected as `nick` and joined both rooms.
    fn in_both_rooms<'a>(config: &'a Config, nick: &str) -> Session<'a> {
        let mut session = Session::new(config);
        session.handle(&message(&format!(":irc.example.net 001 {nick} :Welcome")));
        session.handle(&message(&format!(":{nick}!lurker@host JOIN #purgatory")));
        session.handle(&message(&format!(
            ":{nick}!lurker@host JOIN :#FBI_watchlist"
        )));
        session
    }

    #[test]
    fn waits_for_the_welcome_before_rejoining() {
        let config = Config::example();
        let mut session = Session::new(&config);
        assert!(session.rejoin().is_empty());

        session.handle(&message(":irc.example.net 001 lurker :Welcome"));
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist")]
        );
    }

    #[test]
    fn does_not_rejoin_rooms_it_is_in() {
        let config = Config::example();
        let session = in_both_rooms(&config, "lurker");
        assert!(session.rejoin().is_empty());
    }

    #[test]
    fn matches_rooms_regardless_of_case() {
        let config = Config::example();
        let mut session = Session::new(&config);
        session.handle(&message(":irc.example.net 001 lurker :Welcome"));
        session.handle(&message(":Lurker!lurker@host JOIN #FBI_Watchlist"));
        assert_eq!(session.rejoin(), [join("#purgatory")]);
    }

    #[test]
    fn ignores_other_users_joining() {
        let config = Config::example();
        let mut session = Session::new(&config);
        session.handle(&message(":irc.example.net 001 lurker :Welcome"));
        session.handle(&message(":alice!alice@host JOIN #purgatory"));
        assert_eq!(session.rejoin().len(), 2);
    }

    #[test]
    fn rejoins_after_a_kick_or_part() {
        let config = Config::example();
        let mut session = in_both_rooms(&config, "lurker");

        session.handle(&message(":op!op@host KICK #purgatory lurker :out"));
        assert_eq!(session.rejoin(), [join("#purgatory")]);

        session.handle(&message(":lurker!lurker@host PART #FBI_watchlist"));
        assert_eq!(
            session.rejoin(),
            [join("#purgatory"), join("#FBI_watchlist")]
        );
    }

    #[test]
    fn ignores_kicks_of_other_users() {
        let config = Config::example();
        let mut session = in_both_rooms(&config, "lurker");
        session.handle(&message(":op!op@host KICK #purgatory alice :out"));
        assert!(session.rejoin().is_empty());
    }

    #[test]
    fn follows_its_own_nick_changes() {
        let config = Config::example();
        let mut session = in_both_rooms(&config, "lurker_");

        let commands = session.handle(&message(":lurker_!lurker@host NICK lurker"));
        assert!(commands.is_empty());
        assert_eq!(session.nick, "lurker");

        session.handle(&message(":op!op@host KICK #purgatory lurker :out"));
        assert_eq!(session.rejoin(), [join("#purgatory")]);
    }

    #[test]
    fn takes_its_nick_back_when_it_is_freed() {
        let config = Config::example();
        let mut session = in_both_rooms(&config, "lurker_");

        let commands = session.handle(&message(":lurker!lurker@old.host QUIT :Ping timeout"));
        assert_eq!(commands, [Command::NICK("lurker".to_owned())]);

        let commands = session.handle(&message(":lurker!someone@host NICK someone"));
        assert_eq!(commands, [Command::NICK("lurker".to_owned())]);
    }

    #[test]
    fn keeps_its_nick_when_it_already_has_it() {
        let config = Config::example();
        let mut session = in_both_rooms(&config, "lurker");
        let commands = session.handle(&message(":alice!alice@host QUIT :bye"));
        assert!(commands.is_empty());
    }
}
