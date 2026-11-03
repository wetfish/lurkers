//! The commands admins send lurker, in private messages or in a channel
//! after its nick, to mark lurkers and record what they know about them.
//! `reload`, `poke`, `join`, `part` and `channels` are handled by the
//! connection, which holds the settings and knows the channels.

use chrono::{Datelike, NaiveDate};

use crate::casemap;
use crate::config::{Room, Rooms};
use crate::dates;
use crate::db::{Db, Frequency, Lurker, Marked};
use crate::presence::Presence;

/// Reply lines are cut at about this many bytes, so they fit in one IRC
/// message with room to spare.
const LINE_LENGTH: usize = 350;

/// What a command needs to know.
pub struct Context<'a> {
    pub db: &'a Db,
    pub rooms: &'a Rooms,
    pub presence: &'a Presence,
    /// The admin's nick, recorded on the lurkers they mark
    pub admin: &'a str,
    /// Seconds since 1970 UTC
    pub now: i64,
}

/// Why a command did nothing.
enum Problem {
    /// The command was wrong; the text says how.
    Usage(String),
    Database(rusqlite::Error),
}

impl From<String> for Problem {
    fn from(text: String) -> Problem {
        Problem::Usage(text)
    }
}

impl From<&str> for Problem {
    fn from(text: &str) -> Problem {
        Problem::Usage(text.to_owned())
    }
}

impl From<rusqlite::Error> for Problem {
    fn from(err: rusqlite::Error) -> Problem {
        Problem::Database(err)
    }
}

type Reply = Result<Vec<String>, Problem>;

/// What a `trusted` or `untrusted` command gives, read in full before
/// anything is changed, so a mistake anywhere in it changes nothing.
struct Fields<'a> {
    /// The grace period, where None inside means never
    grace: Option<Option<i64>>,
    /// The channels, each with its own date or the one given with `since=`
    channels: Vec<(&'a str, NaiveDate)>,
    peak: Option<(i32, Frequency)>,
}

/// Carries out a command and returns the lines to reply with.
pub fn run(context: &Context, text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some((command, args)) = words.split_first() else {
        return help(context.rooms);
    };

    let reply = match command.to_ascii_lowercase().as_str() {
        "trusted" => mark(context, args, Room::Purgatory),
        "untrusted" => mark(context, args, Room::Watchlist),
        "unchannel" => unchannel(context, args),
        "info" => info(context, args),
        "list" => list(context),
        "forget" => forget(context, args),
        "help" => Ok(help(context.rooms)),
        _ => Err(Problem::Usage(format!(
            "I don't know the command {command}. Send \"help\" for the list."
        ))),
    };

    match reply {
        Ok(lines) => lines,
        Err(Problem::Usage(text)) => vec![text],
        Err(Problem::Database(err)) => {
            eprintln!("error: database: {err}");
            vec![
                "Something went wrong with the database, so nothing more changed. lurker's log \
                 has the details."
                    .to_owned(),
            ]
        }
    }
}

pub fn help(rooms: &Rooms) -> Vec<String> {
    let purgatory = rooms.channel(Room::Purgatory);
    let watchlist = rooms.channel(Room::Watchlist);
    vec![
        format!(
            "trusted <nick> grace=<time> [#channel=<date> ...] [since=<date>] \
             [peak=<year>/<daily|weekly|monthly>]: mark or update a lurker for {purgatory}, \
             someone we know who hasn't been around"
        ),
        format!(
            "untrusted <nick> grace=<time> [#channel=<date> ...] [since=<date>]: mark or update \
             a lurker for {watchlist}, someone nobody can vouch for"
        ),
        "Fields go in any order, and only the ones given change. grace is how long after a \
         warning I kick them from my channels: 90m, 12h, 3d, 2w or never"
            .to_owned(),
        "#channel=<date> is when they went silent there: when they last spoke, or showed up if \
         they never did. Dates are YYYY-MM-DD or days ago, such as 180d. since is the date for \
         channels given without one, as in since=180d #wetfish #music"
            .to_owned(),
        "unchannel <nick> <#channel>: remove a channel from a lurker".to_owned(),
        "info <nick>: everything recorded about a lurker".to_owned(),
        "list: every lurker, and who is in their room now".to_owned(),
        "forget <nick>: delete a lurker".to_owned(),
        "poke <nick>: send a lurker their weekly ping now".to_owned(),
        "join <#channel>: join a channel, where I warn and kick lurkers once I have +h or +o. \
         Inviting me does the same"
            .to_owned(),
        "part <#channel>: leave a channel".to_owned(),
        "channels: the channels I'm in, and where I can kick".to_owned(),
        "reload: read the settings file again".to_owned(),
    ]
}

fn mark(context: &Context, args: &[&str], room: Room) -> Reply {
    let trust = trust(room);
    let Some((nick, fields)) = args.split_first() else {
        return Err(usage(room).into());
    };
    let nick = check_nick(nick)?;
    let today = dates::today(context.now);
    let fields = Fields::parse(fields, room, today)?;
    if fields.grace.is_none() && context.db.lurker(nick)?.is_none() {
        return Err(format!(
            "Give a grace period too, such as: {trust} {nick} grace=3d. It is how long after a \
             warning I kick them from my channels, or never."
        )
        .into());
    }

    let channel = context.rooms.channel(room);
    let here = context.presence.contains(channel, nick);
    let marked = context
        .db
        .mark(nick, room, here, context.admin, context.now)?;
    let id = find(context, nick)?.id;
    if let Some(grace) = fields.grace {
        context.db.set_grace(id, grace)?;
    }
    for (name, since) in &fields.channels {
        context.db.set_channel(id, name, *since)?;
    }
    if let Some((year, frequency)) = fields.peak {
        context.db.set_peak(id, year, frequency)?;
    }

    let mut first = match marked {
        Marked::New => format!("{nick} is marked {trust} ({channel})."),
        Marked::Moved => format!(
            "{nick} is marked {trust} ({channel}) instead of {} ({}).",
            self::trust(other(room)),
            context.rooms.channel(other(room))
        ),
        Marked::Unchanged => format!("{nick} is already marked {trust} ({channel})."),
    };
    if let Some(grace) = fields.grace {
        first.push_str(&format!(" {}.", capitalize(&kicked(grace))));
    }
    if let Some((year, frequency)) = fields.peak {
        first.push_str(&format!(" Chatted {} in {year}.", frequency.phrase()));
    }
    if marked != Marked::Unchanged && !here {
        first.push_str(&format!(
            " Their time in {channel} starts counting when they join it."
        ));
    }

    let mut lines = vec![first];
    if !fields.channels.is_empty() {
        let channels = fields
            .channels
            .iter()
            .map(|(name, since)| describe_channel(name, *since, today))
            .collect();
        lines.extend(wrap("Channels: ", channels));
    }
    Ok(lines)
}

impl<'a> Fields<'a> {
    fn parse(args: &[&'a str], room: Room, today: NaiveDate) -> Result<Fields<'a>, Problem> {
        let mut grace = None;
        let mut since = None;
        let mut peak = None;
        let mut channels: Vec<(&str, Option<NaiveDate>)> = Vec::new();

        for &arg in args {
            if arg.starts_with('#') {
                let (name, date) = match arg.split_once('=') {
                    Some((name, date)) => (name, Some(dates::parse(date, today)?)),
                    None => (arg, None),
                };
                let name = check_channel(name)?;
                if channels.iter().any(|(seen, _)| casemap::eq(seen, name)) {
                    return Err(format!("{name} is given twice.").into());
                }
                channels.push((name, date));
                continue;
            }

            let Some((key, value)) = arg.split_once('=') else {
                return Err(format!(
                    "{arg} isn't a field. Fields look like grace=3d, since=180d, #wetfish=180d \
                     and peak=2022/daily."
                )
                .into());
            };
            let key = key.to_ascii_lowercase();
            let given_twice = match key.as_str() {
                "grace" => grace.replace(dates::parse_grace(value)?).is_some(),
                "since" => since.replace(dates::parse(value, today)?).is_some(),
                "peak" if room == Room::Watchlist => {
                    return Err("peak is only for trusted lurkers.".into());
                }
                "peak" => peak.replace(parse_peak(value, today)?).is_some(),
                _ => {
                    return Err(format!(
                        "I don't know the field {key}. Fields are grace, since, #channel and \
                         peak."
                    )
                    .into());
                }
            };
            if given_twice {
                return Err(format!("{key} is given twice.").into());
            }
        }

        let mut used_since = false;
        let mut dated = Vec::new();
        for (name, date) in channels {
            let date = match (date, since) {
                (Some(date), _) => date,
                (None, Some(since)) => {
                    used_since = true;
                    since
                }
                (None, None) => {
                    return Err(format!(
                        "Give a date for {name}, as {name}=180d, or one for every channel \
                         without its own with since=180d."
                    )
                    .into());
                }
            };
            dated.push((name, date));
        }
        if since.is_some() && !used_since {
            return Err(
                "since only gives a date to channels listed without one, as in since=180d \
                 #wetfish."
                    .into(),
            );
        }

        Ok(Fields {
            grace,
            channels: dated,
            peak,
        })
    }
}

/// A peak given as `year/how often`, such as `2022/daily`.
fn parse_peak(value: &str, today: NaiveDate) -> Result<(i32, Frequency), Problem> {
    let (year, frequency) = value
        .split_once('/')
        .ok_or("Give peak as year/how often, such as peak=2022/daily.")?;
    let this_year = today.year();
    let year: i32 = year
        .parse()
        .ok()
        .filter(|year| (1988..=this_year).contains(year))
        .ok_or_else(|| format!("Give the peak year as a number from 1988 to {this_year}."))?;
    let frequency =
        Frequency::parse(frequency).ok_or("Give how often as daily, weekly or monthly.")?;
    Ok((year, frequency))
}

fn usage(room: Room) -> String {
    match room {
        Room::Purgatory => "Usage: trusted <nick> grace=<time> [#channel=<date> ...] \
                            [since=<date>] [peak=<year>/<daily|weekly|monthly>]"
            .to_owned(),
        Room::Watchlist => {
            "Usage: untrusted <nick> grace=<time> [#channel=<date> ...] [since=<date>]".to_owned()
        }
    }
}

fn unchannel(context: &Context, args: &[&str]) -> Reply {
    let [nick, channel] = args else {
        return Err("Usage: unchannel <nick> <#channel>".into());
    };
    let lurker = find(context, nick)?;
    let channel = check_channel(channel)?;

    if context.db.remove_channel(lurker.id, channel)? {
        Ok(vec![format!("{}: removed {channel}.", lurker.nick)])
    } else {
        Ok(vec![format!("{} has no {channel}.", lurker.nick)])
    }
}

fn info(context: &Context, args: &[&str]) -> Reply {
    let [nick] = args else {
        return Err("Usage: info <nick>".into());
    };
    let lurker = find(context, nick)?;
    let today = dates::today(context.now);
    let channel = context.rooms.channel(lurker.room);
    let name = &lurker.nick;
    let trust = trust(lurker.room);

    let here = context.presence.contains(channel, name);
    let time = match lurker.here_since {
        Some(since) => format!(
            "{} since first seen there",
            dates::days((context.now - since).max(0) / 86_400)
        ),
        None => "not seen there yet".to_owned(),
    };
    let mut lines = vec![format!(
        "{name}: {trust} ({channel}), {}, {time}. Marked by {} on {}.",
        if here {
            "in the room now"
        } else {
            "not in the room now"
        },
        lurker.marked_by,
        dates::today(lurker.marked_at)
    )];

    if lurker.channels.is_empty() {
        lines.push(format!(
            "No channels yet. Add them with: {trust} {name} #channel=<date>"
        ));
    } else {
        let channels = lurker
            .channels
            .iter()
            .map(|idle| describe_channel(&idle.channel, idle.since, today))
            .collect();
        lines.extend(wrap("Channels: ", channels));
    }

    lines.push(match (lurker.next_ping_at, lurker.here_since) {
        (Some(at), _) if at > context.now => format!(
            "Next ping: around {} (in {}).",
            dates::today(at),
            dates::days((at - context.now) / 86_400)
        ),
        (Some(_), _) => "Next ping: due, as soon as they're in the room.".to_owned(),
        (None, Some(_)) => "Next ping: being scheduled.".to_owned(),
        (None, None) => format!("Next ping: after they first join {channel}."),
    });

    lines.push(match (lurker.grace, lurker.kick_at()) {
        (None, _) => "Kicks: never kicked from my channels.".to_owned(),
        (Some(grace), None) => format!(
            "Kicks: warned when I see them in a channel where I can kick, and kicked {} later.",
            dates::length(grace)
        ),
        (Some(_), Some(at)) if at > context.now => format!(
            "Kicks: warned on {}, kicked from my channels in {}.",
            dates::today(lurker.warned_at.unwrap_or(at)),
            dates::about(at - context.now)
        ),
        (Some(_), Some(_)) => format!(
            "Kicks: warned on {}, kicked from my channels on sight.",
            dates::today(lurker.warned_at.unwrap_or_default())
        ),
    });

    if lurker.room == Room::Purgatory {
        lines.push(match lurker.peak {
            Some((year, frequency)) => {
                format!("Most active: chatted {} in {year}.", frequency.phrase())
            }
            None => format!(
                "Most active: unknown. Add it with: trusted {name} \
                 peak=<year>/<daily|weekly|monthly>"
            ),
        });
    }

    // Everything above as one command, to copy, change and send back
    let command = format!("As one command: {}", as_command(&lurker));
    if command.len() <= LINE_LENGTH {
        lines.push(command);
    }
    Ok(lines)
}

/// The `trusted` or `untrusted` command that records everything about a
/// lurker.
fn as_command(lurker: &Lurker) -> String {
    let grace = match lurker.grace {
        Some(grace) => dates::short(grace),
        None => "never".to_owned(),
    };
    let mut command = format!("{} {} grace={grace}", trust(lurker.room), lurker.nick);
    for idle in &lurker.channels {
        command.push_str(&format!(" {}={}", idle.channel, idle.since));
    }
    if let (Room::Purgatory, Some((year, frequency))) = (lurker.room, lurker.peak) {
        command.push_str(&format!(" peak={year}/{}", frequency.as_str()));
    }
    command
}

fn list(context: &Context) -> Reply {
    let lurkers = context.db.lurkers()?;
    let mut lines = Vec::new();

    for room in Room::ALL {
        let nicks: Vec<String> = lurkers
            .iter()
            .filter(|lurker| lurker.room == room)
            .map(|lurker| {
                if context
                    .presence
                    .contains(context.rooms.channel(room), &lurker.nick)
                {
                    format!("{} (here)", lurker.nick)
                } else {
                    lurker.nick.clone()
                }
            })
            .collect();
        let prefix = format!("{} ({}): ", context.rooms.channel(room), nicks.len());
        if nicks.is_empty() {
            lines.push(format!("{prefix}nobody"));
        } else {
            lines.extend(wrap(&prefix, nicks));
        }
    }
    Ok(lines)
}

fn forget(context: &Context, args: &[&str]) -> Reply {
    let [nick] = args else {
        return Err("Usage: forget <nick>".into());
    };
    if context.db.forget(nick)? {
        Ok(vec![format!("Forgot {nick}.")])
    } else {
        Ok(vec![format!("{nick} isn't marked.")])
    }
}

/// The lurker with a nick, or a reply saying how to mark them.
fn find(context: &Context, nick: &str) -> Result<Lurker, Problem> {
    let nick = check_nick(nick)?;
    context.db.lurker(nick)?.ok_or_else(|| {
        Problem::Usage(format!(
            "{nick} isn't marked. Mark them first with: untrusted {nick} grace=<time>, or \
             trusted {nick} grace=<time>."
        ))
    })
}

fn check_nick(nick: &str) -> Result<&str, Problem> {
    if nick.starts_with(['#', '&']) || nick.contains([',', '!', '@', '*', '=']) {
        return Err(format!("{nick} isn't a nick.").into());
    }
    Ok(nick)
}

fn check_channel(channel: &str) -> Result<&str, Problem> {
    if !channel.starts_with('#') || channel.len() < 2 || channel.contains(',') {
        return Err(format!("{channel} isn't a channel. Channels start with #.").into());
    }
    Ok(channel)
}

/// A channel and when the lurker went silent there, such as "#wetfish
/// silent since 2026-05-06 (180 days)".
fn describe_channel(channel: &str, since: NaiveDate, today: NaiveDate) -> String {
    format!(
        "{channel} silent since {since} ({})",
        dates::days(dates::days_since(since, today))
    )
}

/// How a lurker is kicked, such as "kicked from my channels 3 days after
/// being warned".
fn kicked(grace: Option<i64>) -> String {
    match grace {
        Some(grace) => format!(
            "kicked from my channels {} after being warned",
            dates::length(grace)
        ),
        None => "never kicked from my channels".to_owned(),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn trust(room: Room) -> &'static str {
    match room {
        Room::Purgatory => "trusted",
        Room::Watchlist => "untrusted",
    }
}

fn other(room: Room) -> Room {
    match room {
        Room::Purgatory => Room::Watchlist,
        Room::Watchlist => Room::Purgatory,
    }
}

/// Joins items with commas into lines of at most about `LINE_LENGTH`
/// bytes, the first starting with `prefix`.
fn wrap(prefix: &str, items: Vec<String>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = prefix.to_owned();
    let mut empty = true;

    for item in items {
        if !empty && line.len() + 2 + item.len() > LINE_LENGTH {
            lines.push(line);
            line = String::new();
            empty = true;
        }
        if !empty {
            line.push_str(", ");
        }
        line.push_str(&item);
        empty = false;
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// 2026-11-02 12:00 UTC
    const NOW: i64 = 1_793_620_800;
    const DAY: i64 = 86_400;

    struct Fixture {
        db: Db,
        config: Config,
        presence: Presence,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                db: Db::open_in_memory(),
                config: Config::example(),
                presence: Presence::default(),
            }
        }

        fn run(&self, text: &str) -> Vec<String> {
            let context = Context {
                db: &self.db,
                rooms: &self.config.rooms,
                presence: &self.presence,
                admin: "rachel",
                now: NOW,
            };
            run(&context, text)
        }

        fn lurker(&self, nick: &str) -> Lurker {
            self.db.lurker(nick).unwrap().unwrap()
        }

        /// A lurker's channels as `#channel=date`.
        fn channels(&self, nick: &str) -> Vec<String> {
            self.lurker(nick)
                .channels
                .iter()
                .map(|idle| format!("{}={}", idle.channel, idle.since))
                .collect()
        }
    }

    #[test]
    fn marks_lurkers_with_everything_at_once() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");

        assert_eq!(
            fixture
                .run("trusted alice grace=3d #wetfish=348d #dryfish=2026-11-01 peak=2022/weekly"),
            [
                "alice is marked trusted (#purgatory). Kicked from my channels 3 days after being \
                 warned. Chatted every week in 2022.",
                "Channels: #wetfish silent since 2025-11-19 (348 days), #dryfish silent since \
                 2026-11-01 (1 day)",
            ]
        );

        let alice = fixture.lurker("alice");
        assert_eq!(alice.room, Room::Purgatory);
        assert_eq!(alice.here_since, Some(NOW));
        assert_eq!(alice.marked_by, "rachel");
        assert_eq!(alice.grace, Some(3 * DAY));
        assert_eq!(alice.peak, Some((2022, Frequency::Weekly)));
        assert_eq!(
            fixture.channels("alice"),
            ["#dryfish=2026-11-01", "#wetfish=2025-11-19"]
        );
    }

    #[test]
    fn takes_fields_in_any_order() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("untrusted bob #music since=180d #wetfish=95d GRACE=12h"),
            [
                "bob is marked untrusted (#FBI_watchlist). Kicked from my channels 12 hours after \
                 being warned. Their time in #FBI_watchlist starts counting when they join it.",
                "Channels: #music silent since 2026-05-06 (180 days), #wetfish silent since \
                 2026-07-30 (95 days)",
            ]
        );
        assert_eq!(fixture.lurker("bob").grace, Some(12 * 3600));
        assert_eq!(
            fixture.channels("bob"),
            ["#music=2026-05-06", "#wetfish=2026-07-30"]
        );
    }

    #[test]
    fn updates_only_what_is_given() {
        let fixture = Fixture::new();
        fixture.run("untrusted bob grace=3d #wetfish=180d");

        assert_eq!(
            fixture.run("untrusted bob grace=never #music=10d #WETFISH=95d"),
            [
                "bob is already marked untrusted (#FBI_watchlist). Never kicked from my channels.",
                "Channels: #music silent since 2026-10-23 (10 days), #WETFISH silent since \
                 2026-07-30 (95 days)",
            ]
        );
        assert_eq!(fixture.lurker("bob").grace, None);
        assert_eq!(
            fixture.channels("bob"),
            ["#music=2026-10-23", "#WETFISH=2026-07-30"]
        );

        assert_eq!(
            fixture.run("untrusted bob"),
            ["bob is already marked untrusted (#FBI_watchlist)."]
        );
        assert_eq!(fixture.channels("bob").len(), 2);
    }

    #[test]
    fn moves_lurkers_between_rooms() {
        let fixture = Fixture::new();
        fixture.run("untrusted bob grace=3d #wetfish=180d");
        assert_eq!(
            fixture.run("trusted bob"),
            [
                "bob is marked trusted (#purgatory) instead of untrusted (#FBI_watchlist). Their \
                 time in #purgatory starts counting when they join it."
            ]
        );
        let bob = fixture.lurker("bob");
        assert_eq!(bob.room, Room::Purgatory);
        assert_eq!(bob.grace, Some(3 * DAY));
        assert_eq!(bob.channels.len(), 1);
    }

    #[test]
    fn needs_a_grace_period_to_mark_someone_new() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("trusted alice #wetfish=1d"),
            [
                "Give a grace period too, such as: trusted alice grace=3d. It is how long after \
                 a warning I kick them from my channels, or never."
            ]
        );
        assert_eq!(fixture.db.lurker("alice").unwrap(), None);
    }

    #[test]
    fn mistakes_change_nothing() {
        let fixture = Fixture::new();
        for (command, reply) in [
            (
                "untrusted bob grace=3d peak=2022/daily",
                "peak is only for trusted lurkers.",
            ),
            (
                "trusted bob grace=soon",
                "soon isn't a grace period. Give one in minutes, hours, days or weeks, such as \
                 90m, 12h, 3d or 2w, or never",
            ),
            (
                "trusted bob grace=3d #wetfish",
                "Give a date for #wetfish, as #wetfish=180d, or one for every channel without its \
                 own with since=180d.",
            ),
            (
                "trusted bob grace=3d since=10d #wetfish=1d",
                "since only gives a date to channels listed without one, as in since=180d \
                 #wetfish.",
            ),
            ("trusted bob grace=3d grace=1d", "grace is given twice."),
            (
                "trusted bob grace=3d #wetfish=1d #WETFISH=2d",
                "#WETFISH is given twice.",
            ),
            (
                "trusted bob grace=3d color=blue",
                "I don't know the field color. Fields are grace, since, #channel and peak.",
            ),
            (
                "trusted bob grace=3d 180d",
                "180d isn't a field. Fields look like grace=3d, since=180d, #wetfish=180d and \
                 peak=2022/daily.",
            ),
            (
                "trusted bob grace=3d #wetfish=2027-01-01",
                "2027-01-01 is in the future",
            ),
            (
                "trusted bob grace=3d #=1d",
                "# isn't a channel. Channels start with #.",
            ),
            (
                "trusted bob grace=3d peak=2030/daily",
                "Give the peak year as a number from 1988 to 2026.",
            ),
            (
                "trusted bob grace=3d peak=2022/hourly",
                "Give how often as daily, weekly or monthly.",
            ),
            (
                "trusted bob grace=3d peak=2022",
                "Give peak as year/how often, such as peak=2022/daily.",
            ),
            (
                "trusted",
                "Usage: trusted <nick> grace=<time> [#channel=<date> ...] [since=<date>] \
                 [peak=<year>/<daily|weekly|monthly>]",
            ),
            ("trusted #wetfish grace=3d", "#wetfish isn't a nick."),
            ("trusted grace=3d", "grace=3d isn't a nick."),
        ] {
            assert_eq!(fixture.run(command), [reply], "{command}");
        }
        assert_eq!(fixture.db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn removes_channels() {
        let fixture = Fixture::new();
        fixture.run("trusted alice grace=3d #wetfish=1d #dryfish=1d");
        assert_eq!(
            fixture.run("unchannel alice #WETFISH"),
            ["alice: removed #WETFISH."]
        );
        assert_eq!(
            fixture.run("unchannel alice #wetfish"),
            ["alice has no #wetfish."]
        );
        assert_eq!(fixture.channels("alice"), ["#dryfish=2026-11-01"]);
    }

    #[test]
    fn shows_info() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");
        fixture.run("trusted alice grace=3d #wetfish=348d #dryfish=1d peak=2022/weekly");

        assert_eq!(
            fixture.run("info ALICE"),
            [
                "alice: trusted (#purgatory), in the room now, 0 days since first seen there. \
                 Marked by rachel on 2026-11-02.",
                "Channels: #dryfish silent since 2026-11-01 (1 day), #wetfish silent since \
                 2025-11-19 (348 days)",
                "Next ping: being scheduled.",
                "Kicks: warned when I see them in a channel where I can kick, and kicked 3 days \
                 later.",
                "Most active: chatted every week in 2022.",
                "As one command: trusted alice grace=3d #dryfish=2026-11-01 #wetfish=2025-11-19 \
                 peak=2022/weekly",
            ]
        );

        fixture.run("untrusted bob grace=never");
        assert_eq!(
            fixture.run("info bob"),
            [
                "bob: untrusted (#FBI_watchlist), not in the room now, not seen there yet. \
                 Marked by rachel on 2026-11-02.",
                "No channels yet. Add them with: untrusted bob #channel=<date>",
                "Next ping: after they first join #FBI_watchlist.",
                "Kicks: never kicked from my channels.",
                "As one command: untrusted bob grace=never",
            ]
        );
    }

    #[test]
    fn the_info_command_marks_the_same_lurker() {
        let fixture = Fixture::new();
        fixture.run("trusted alice grace=90m since=348d #wetfish #dryfish peak=2022/daily");
        let info = fixture.run("info alice");
        let command = info
            .last()
            .unwrap()
            .strip_prefix("As one command: ")
            .unwrap();
        let before = fixture.lurker("alice");

        fixture.run("forget alice");
        fixture.run(command);
        let after = fixture.lurker("alice");
        assert_eq!(
            (&after.channels, after.grace, after.peak, after.room),
            (&before.channels, before.grace, before.peak, before.room)
        );
    }

    #[test]
    fn shows_when_the_next_ping_comes() {
        let fixture = Fixture::new();
        fixture.run("trusted alice grace=3d");
        let id = fixture.lurker("alice").id;

        fixture.db.set_next_ping(id, NOW + 3 * DAY + 600).unwrap();
        let info = fixture.run("info alice");
        assert_eq!(info[2], "Next ping: around 2026-11-05 (in 3 days).");

        fixture.db.set_next_ping(id, NOW - 60).unwrap();
        let info = fixture.run("info alice");
        assert_eq!(info[2], "Next ping: due, as soon as they're in the room.");
    }

    #[test]
    fn shows_when_the_kick_comes() {
        let fixture = Fixture::new();
        fixture.run("untrusted bob grace=3d");
        let id = fixture.lurker("bob").id;

        fixture.db.set_warned(id, NOW - DAY).unwrap();
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: warned on 2026-11-01, kicked from my channels in 2 days."
        );

        fixture.db.set_warned(id, NOW - 4 * DAY).unwrap();
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: warned on 2026-10-29, kicked from my channels on sight."
        );
    }

    #[test]
    fn lists_lurkers_by_room() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "carol");
        fixture.run("trusted carol grace=3d");
        fixture.run("trusted alice grace=3d");

        assert_eq!(
            fixture.run("list"),
            [
                "#purgatory (2): alice, carol (here)",
                "#FBI_watchlist (0): nobody"
            ]
        );
    }

    #[test]
    fn forgets_lurkers() {
        let fixture = Fixture::new();
        fixture.run("trusted alice grace=3d");
        assert_eq!(fixture.run("forget alice"), ["Forgot alice."]);
        assert_eq!(fixture.run("forget alice"), ["alice isn't marked."]);
    }

    #[test]
    fn explains_mistakes() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("info nobody"),
            [
                "nobody isn't marked. Mark them first with: untrusted nobody grace=<time>, or \
                 trusted nobody grace=<time>."
            ]
        );
        for removed in [
            "grace bob 3d",
            "channel bob #wetfish 1d",
            "peak bob 2022 daily",
        ] {
            let reply = fixture.run(removed);
            assert!(
                reply[0].starts_with("I don't know the command"),
                "{reply:?}"
            );
        }
        assert_eq!(fixture.run("help").len(), 13);
        assert_eq!(fixture.run("").len(), 13);
        assert!(
            fixture
                .run("help")
                .iter()
                .all(|line| line.len() <= LINE_LENGTH)
        );
    }

    #[test]
    fn wraps_long_lists() {
        let items: Vec<String> = (0..100).map(|n| format!("lurker{n}")).collect();
        let lines = wrap("Lurkers: ", items);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.len() <= LINE_LENGTH));
        assert!(lines[0].starts_with("Lurkers: lurker0, lurker1"));
    }
}
