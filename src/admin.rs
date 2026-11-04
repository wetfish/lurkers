//! The commands admins send lurker, in private messages or in a channel
//! after its nick, to mark lurkers and record what they know about them,
//! and to keep the nudge list. `reload`, `poke`, `join`, `part` and
//! `channels` are handled by the connection, which holds the settings and
//! knows the channels.

use chrono::{Datelike, NaiveDate};

use crate::config::{Room, Rooms};
use crate::dates;
use crate::db::{Db, Frequency, Lurker, Marked, Nudge, NudgeEnd};
use crate::presence::Presence;

/// Reply lines are cut at about this many bytes, so they fit in one IRC
/// message with room to spare.
const LINE_LENGTH: usize = 350;

/// Every command's name, including those the connection carries out.
const COMMANDS: [&str; 12] = [
    "trusted",
    "untrusted",
    "info",
    "list",
    "forget",
    "nudge",
    "poke",
    "join",
    "part",
    "channels",
    "reload",
    "help",
];

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// The shortest and longest a nudge can last, to catch typos.
const NUDGE_LENGTHS: std::ops::RangeInclusive<i64> = HOUR..=365 * DAY;

/// The most reminders a nudge can have.
const MAX_REMINDERS: i64 = 20;

/// How far apart a nudge's reminders must be at least.
const REMINDER_GAP: i64 = HOUR;

/// How many ended nudges `nudge list` shows, newest first.
const ENDED_SHOWN: usize = 20;

const NUDGE_USAGE: &str = "Usage: nudge user=<nick> for=<time> reminders=<number>, nudge list, \
                           or nudge remove user=<nick>";

/// Whether a word is the name of a command, ignoring case.
pub fn is_command(word: &str) -> bool {
    COMMANDS
        .iter()
        .any(|command| command.eq_ignore_ascii_case(word))
}

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
struct Fields {
    /// The grace period, where None inside means never
    grace: Option<Option<i64>>,
    /// When they last spoke anywhere
    silent: Option<NaiveDate>,
    /// The year they were most active, and how often they chatted then
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
        "info" => info(context, args),
        "list" => list(context),
        "forget" => forget(context, args),
        "nudge" => nudge(context, args),
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
            "trusted <nick> grace=<time> [silent=<date>] [activity=<daily|weekly|monthly> \
             year=<year>]: mark or update a lurker for {purgatory}, someone we know who hasn't \
             been around"
        ),
        format!(
            "untrusted <nick> grace=<time> [silent=<date>]: mark or update a lurker for \
             {watchlist}, someone nobody can vouch for"
        ),
        "Fields go in any order, and only the ones given change. grace is how long after a \
         warning I kick them from every channel and sajoin them into their room: 90m, 12h, 3d, \
         2w, 0 to kick them without a warning, or never. Giving it starts the warning over"
            .to_owned(),
        "silent is when they last spoke anywhere, or showed up if they never did: YYYY-MM-DD \
         or days ago, such as 180d. activity and year go together, for trusted lurkers: how \
         often they chatted in the year they were most active"
            .to_owned(),
        "info <nick>: everything recorded about a lurker".to_owned(),
        "list: every lurker, and who is in their room now".to_owned(),
        "forget <nick>: delete a lurker".to_owned(),
        "nudge user=<nick> for=<time> reminders=<number>: remind someone to chat, in every \
         channel they're in, such as nudge user=alice for=3w reminders=5. nudge list: who is \
         being nudged and how it went. nudge remove user=<nick>: stop"
            .to_owned(),
        "poke <nick>: send a lurker their weekly ping now".to_owned(),
        "join <#channel>: join a channel and op myself there, to watch for lurkers. Inviting \
         me does the same"
            .to_owned(),
        "part <#channel>: leave a channel".to_owned(),
        "channels: the channels I'm in, and whether I'm opped there".to_owned(),
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
             warning I kick them from every channel and sajoin them into their room, or never."
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
    if let Some(silent) = fields.silent {
        context.db.set_silent(id, silent)?;
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
    if let Some(silent) = fields.silent {
        first.push_str(&format!(" {}.", capitalize(&silent_since(silent, today))));
    }
    if let Some((year, frequency)) = fields.peak {
        first.push_str(&format!(" Chatted {} in {year}.", frequency.phrase()));
    }
    if marked != Marked::Unchanged && !here {
        first.push_str(&format!(
            " Their time in {channel} starts counting when they join it."
        ));
    }
    // Lurkers aren't nudged
    if context
        .db
        .end_nudge(nick, NudgeEnd::Marked, Some(context.admin), context.now)?
        .is_some()
    {
        first.push_str(" Removed from the nudge list.");
    }

    Ok(vec![first])
}

impl Fields {
    fn parse(args: &[&str], room: Room, today: NaiveDate) -> Result<Fields, Problem> {
        let mut grace = None;
        let mut silent = None;
        let mut activity = None;
        let mut year = None;

        for &arg in args {
            let Some((key, value)) = arg.split_once('=') else {
                return Err(format!(
                    "{arg} isn't a field. Fields look like grace=3d, silent=180d, activity=daily \
                     and year=2022."
                )
                .into());
            };
            let key = key.to_ascii_lowercase();
            let given_twice = match key.as_str() {
                "grace" => grace.replace(dates::parse_grace(value)?).is_some(),
                "silent" => silent.replace(dates::parse(value, today)?).is_some(),
                "activity" | "year" if room == Room::Watchlist => {
                    return Err("activity and year are only for trusted lurkers.".into());
                }
                "activity" => activity.replace(parse_activity(value)?).is_some(),
                "year" => year.replace(parse_year(value, today)?).is_some(),
                _ => {
                    return Err(format!(
                        "I don't know the field {key}. Fields are grace, silent, activity and \
                         year."
                    )
                    .into());
                }
            };
            if given_twice {
                return Err(format!("{key} is given twice.").into());
            }
        }

        let peak = match (year, activity) {
            (Some(year), Some(activity)) => Some((year, activity)),
            (None, None) => None,
            _ => {
                return Err(
                    "Give activity and year together, such as activity=daily year=2022.".into(),
                );
            }
        };

        Ok(Fields {
            grace,
            silent,
            peak,
        })
    }
}

/// How often a lurker chatted in the year they were most active.
fn parse_activity(value: &str) -> Result<Frequency, Problem> {
    Frequency::parse(value).ok_or_else(|| "Give activity as daily, weekly or monthly.".into())
}

/// The year a lurker was most active.
fn parse_year(value: &str, today: NaiveDate) -> Result<i32, Problem> {
    let this_year = today.year();
    value
        .parse::<i32>()
        .ok()
        .filter(|year| (1988..=this_year).contains(year))
        .ok_or_else(|| format!("Give year as a number from 1988 to {this_year}.").into())
}

fn usage(room: Room) -> String {
    match room {
        Room::Purgatory => "Usage: trusted <nick> grace=<time> [silent=<date>] \
                            [activity=<daily|weekly|monthly> year=<year>]"
            .to_owned(),
        Room::Watchlist => "Usage: untrusted <nick> grace=<time> [silent=<date>]".to_owned(),
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

    lines.push(match lurker.silent {
        Some(silent) => format!("{}.", capitalize(&silent_since(silent, today))),
        None => format!("Silent since: unknown. Add it with: {trust} {name} silent=<date>"),
    });

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

    let warned = dates::today(lurker.warned_at.unwrap_or_default());
    lines.push(match (lurker.grace, lurker.kick_at()) {
        (None, _) => "Kicks: never warned or kicked.".to_owned(),
        _ if lurker.released() => format!(
            "Kicks: stopped, since they spoke on {} after {}. A new grace period starts them \
             again.",
            dates::today(lurker.spoke_at.unwrap_or_default()),
            if lurker.grace == Some(0) {
                "the kicks started"
            } else {
                "being warned"
            }
        ),
        (Some(0), None) => {
            "Kicks: kicked without a warning the next time I find them online.".to_owned()
        }
        (Some(0), Some(_)) => format!(
            "Kicks: since {warned}, without a warning, kicked from every channel and sajoined \
             into {channel} whenever they're online."
        ),
        (Some(grace), None) => format!(
            "Kicks: warned the next time I find them online, and kicked {} later.",
            dates::length(grace)
        ),
        (Some(_), Some(at)) if at > context.now => format!(
            "Kicks: warned on {warned}, kicked from every channel in {}.",
            dates::about(at - context.now)
        ),
        (Some(_), Some(_)) => format!(
            "Kicks: warned on {warned}, kicked from every channel and sajoined into {channel} \
             whenever they're online."
        ),
    });

    if lurker.room == Room::Purgatory {
        lines.push(match lurker.peak {
            Some((year, frequency)) => {
                format!("Most active: chatted {} in {year}.", frequency.phrase())
            }
            None => format!(
                "Most active: unknown. Add it with: trusted {name} \
                 activity=<daily|weekly|monthly> year=<year>"
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
    if let Some(silent) = lurker.silent {
        command.push_str(&format!(" silent={silent}"));
    }
    if let (Room::Purgatory, Some((year, frequency))) = (lurker.room, lurker.peak) {
        command.push_str(&format!(" activity={} year={year}", frequency.as_str()));
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

/// `nudge user=… for=… reminders=…`, `nudge list` and `nudge remove
/// user=…`.
fn nudge(context: &Context, args: &[&str]) -> Reply {
    match args
        .first()
        .map(|word| word.to_ascii_lowercase())
        .as_deref()
    {
        Some("list") if args.len() == 1 => nudge_list(context),
        Some("remove") => nudge_remove(context, &args[1..]),
        _ => nudge_add(context, args),
    }
}

/// Puts someone on the nudge list.
fn nudge_add(context: &Context, args: &[&str]) -> Reply {
    let fields = named(args, &["user", "for", "reminders"])?;
    let (Some(user), Some(length), Some(reminders)) = (
        field(&fields, "user"),
        field(&fields, "for"),
        field(&fields, "reminders"),
    ) else {
        return Err(NUDGE_USAGE.into());
    };
    let nick = check_nick(user)?;
    let length = dates::parse_length(length)?;
    if !NUDGE_LENGTHS.contains(&length) {
        return Err(format!(
            "for must be from 1 hour to {}.",
            dates::length(*NUDGE_LENGTHS.end())
        )
        .into());
    }
    let reminders: i64 = reminders
        .parse()
        .ok()
        .filter(|count| (1..=MAX_REMINDERS).contains(count))
        .ok_or_else(|| format!("Give reminders as a number from 1 to {MAX_REMINDERS}."))?;
    if length / reminders < REMINDER_GAP {
        return Err(
            "Reminders must be at least an hour apart. Give fewer reminders or a longer for=."
                .into(),
        );
    }

    if let Some(lurker) = context.db.lurker(nick)? {
        return Err(format!(
            "{} is marked {} ({}), and marked lurkers aren't nudged. Send forget {} first to \
             nudge them instead.",
            lurker.nick,
            trust(lurker.room),
            context.rooms.channel(lurker.room),
            lurker.nick
        )
        .into());
    }
    if let Some(going) = context.db.nudge(nick)? {
        return Err(format!(
            "{} is already being nudged: {}. Send nudge remove user={} first to start again.",
            going.nick,
            progress(&going),
            going.nick
        )
        .into());
    }

    context
        .db
        .add_nudge(nick, length, reminders, context.admin, context.now)?;
    let every = if reminders > 1 {
        format!(", about one every {}", dates::about(length / reminders))
    } else {
        String::new()
    };
    Ok(vec![format!(
        "{nick} is on the nudge list: {} over {}{every}, until {}.",
        count(reminders, "reminder"),
        dates::length(length),
        dates::today(context.now + length)
    )])
}

/// Takes someone off the nudge list.
fn nudge_remove(context: &Context, args: &[&str]) -> Reply {
    let fields = named(args, &["user"])?;
    let Some(user) = field(&fields, "user") else {
        return Err("Usage: nudge remove user=<nick>".into());
    };
    let nick = check_nick(user)?;
    match context
        .db
        .end_nudge(nick, NudgeEnd::Removed, Some(context.admin), context.now)?
    {
        Some(nudge) => Ok(vec![format!(
            "Stopped nudging {} after {} of {}.",
            nudge.nick,
            nudge.sent,
            count(nudge.reminders, "reminder")
        )]),
        None => Ok(vec![format!("{nick} isn't being nudged.")]),
    }
}

/// `nudge list`: the nudges going, then the most recent that ended.
fn nudge_list(context: &Context) -> Reply {
    let nudges = context.db.nudges()?;
    let (going, ended): (Vec<&Nudge>, Vec<&Nudge>) =
        nudges.iter().partition(|nudge| nudge.ended_at.is_none());

    let mut lines = Vec::new();
    let prefix = format!("Nudging ({}): ", going.len());
    if going.is_empty() {
        lines.push(format!("{prefix}nobody"));
    } else {
        let items = going
            .iter()
            .map(|nudge| {
                format!(
                    "{} ({}, added by {})",
                    nudge.nick,
                    progress(nudge),
                    nudge.added_by
                )
            })
            .collect();
        lines.extend(wrap(&prefix, items));
    }

    if !ended.is_empty() {
        let prefix = if ended.len() > ENDED_SHOWN {
            format!("Ended, newest {ENDED_SHOWN} of {}: ", ended.len())
        } else {
            "Ended: ".to_owned()
        };
        let items = ended
            .iter()
            .take(ENDED_SHOWN)
            .map(|&nudge| outcome(nudge))
            .collect();
        lines.extend(wrap(&prefix, items));
    }
    Ok(lines)
}

/// How far a nudge has got, such as "2 of 5 reminders, until 2026-11-25".
fn progress(nudge: &Nudge) -> String {
    format!(
        "{} of {}, until {}",
        nudge.sent,
        count(nudge.reminders, "reminder"),
        dates::today(nudge.ends_at())
    )
}

/// How a nudge that ended went, such as "alice spoke 2 days after the
/// first reminder (2026-11-20)".
fn outcome(nudge: &Nudge) -> String {
    let nick = &nudge.nick;
    let how = match nudge.ended {
        Some(NudgeEnd::Spoke) => match (nudge.first_reminder_at, nudge.spoke_at) {
            (Some(first), Some(spoke)) => format!(
                "{nick} spoke {} after the first reminder",
                dates::about(spoke - first)
            ),
            _ => format!("{nick} spoke"),
        },
        Some(NudgeEnd::Finished) => {
            format!("{nick}, no reply after {}", count(nudge.sent, "reminder"))
        }
        Some(NudgeEnd::Removed) => format!(
            "{nick}, removed by {} after {} of {}",
            nudge.ended_by.as_deref().unwrap_or("an admin"),
            nudge.sent,
            count(nudge.reminders, "reminder")
        ),
        Some(NudgeEnd::Marked) => format!(
            "{nick}, marked as a lurker by {}",
            nudge.ended_by.as_deref().unwrap_or("an admin")
        ),
        None => format!("{nick} ({})", progress(nudge)),
    };
    format!(
        "{how} ({})",
        dates::today(nudge.ended_at.unwrap_or(nudge.added_at))
    )
}

/// Fields given as `key=value`, by lowercased key, refusing keys not in
/// `known` and keys given twice.
fn named<'a>(args: &[&'a str], known: &[&str]) -> Result<Vec<(String, &'a str)>, Problem> {
    let mut fields: Vec<(String, &str)> = Vec::new();
    for &arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(format!("{arg} isn't a field. {NUDGE_USAGE}").into());
        };
        let key = key.to_ascii_lowercase();
        if !known.contains(&key.as_str()) {
            return Err(format!("I don't know the field {key}. {NUDGE_USAGE}").into());
        }
        if fields.iter().any(|(seen, _)| *seen == key) {
            return Err(format!("{key} is given twice.").into());
        }
        fields.push((key, value));
    }
    Ok(fields)
}

fn field<'a>(fields: &[(String, &'a str)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| *value)
}

/// A count and a word for what it counts, such as "1 reminder" or "5
/// reminders".
fn count(number: i64, word: &str) -> String {
    if number == 1 {
        format!("1 {word}")
    } else {
        format!("{number} {word}s")
    }
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

/// When a lurker went silent, such as "silent since 2026-05-06 (180
/// days)".
fn silent_since(since: NaiveDate, today: NaiveDate) -> String {
    format!(
        "silent since {since} ({})",
        dates::days(dates::days_since(since, today))
    )
}

/// How a lurker is kicked, such as "kicked from every channel 3 days after
/// being warned".
fn kicked(grace: Option<i64>) -> String {
    match grace {
        Some(0) => "kicked from every channel without a warning".to_owned(),
        Some(grace) => format!(
            "kicked from every channel {} after being warned",
            dates::length(grace)
        ),
        None => "never warned or kicked".to_owned(),
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

        /// When a lurker went silent, as `YYYY-MM-DD`.
        fn silent(&self, nick: &str) -> Option<String> {
            self.lurker(nick).silent.map(|date| date.to_string())
        }
    }

    #[test]
    fn marks_lurkers_with_everything_at_once() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");

        assert_eq!(
            fixture.run("trusted alice grace=3d silent=348d activity=weekly year=2022"),
            [
                "alice is marked trusted (#purgatory). Kicked from every channel 3 days after being \
                 warned. Silent since 2025-11-19 (348 days). Chatted every week in 2022."
            ]
        );

        let alice = fixture.lurker("alice");
        assert_eq!(alice.room, Room::Purgatory);
        assert_eq!(alice.here_since, Some(NOW));
        assert_eq!(alice.marked_by, "rachel");
        assert_eq!(alice.grace, Some(3 * DAY));
        assert_eq!(alice.peak, Some((2022, Frequency::Weekly)));
        assert_eq!(fixture.silent("alice").as_deref(), Some("2025-11-19"));
    }

    #[test]
    fn takes_fields_in_any_order() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("untrusted bob SILENT=2026-05-06 GRACE=12h"),
            [
                "bob is marked untrusted (#FBI_watchlist). Kicked from every channel 12 hours after \
                 being warned. Silent since 2026-05-06 (180 days). Their time in #FBI_watchlist \
                 starts counting when they join it."
            ]
        );
        assert_eq!(fixture.lurker("bob").grace, Some(12 * 3600));
        assert_eq!(fixture.silent("bob").as_deref(), Some("2026-05-06"));

        fixture.run("trusted carol year=2019 grace=1w activity=MONTHLY");
        assert_eq!(
            fixture.lurker("carol").peak,
            Some((2019, Frequency::Monthly))
        );
    }

    #[test]
    fn updates_only_what_is_given() {
        let fixture = Fixture::new();
        fixture.run("trusted alice grace=3d silent=180d activity=daily year=2022");

        assert_eq!(
            fixture.run("trusted alice grace=never silent=95d"),
            [
                "alice is already marked trusted (#purgatory). Never warned or kicked. Silent \
                 since 2026-07-30 (95 days)."
            ]
        );
        let alice = fixture.lurker("alice");
        assert_eq!(alice.grace, None);
        assert_eq!(alice.peak, Some((2022, Frequency::Daily)));
        assert_eq!(fixture.silent("alice").as_deref(), Some("2026-07-30"));

        fixture.run("trusted alice activity=weekly year=2023");
        let alice = fixture.lurker("alice");
        assert_eq!(alice.peak, Some((2023, Frequency::Weekly)));
        assert_eq!(fixture.silent("alice").as_deref(), Some("2026-07-30"));

        assert_eq!(
            fixture.run("trusted alice"),
            ["alice is already marked trusted (#purgatory)."]
        );
        assert_eq!(fixture.silent("alice").as_deref(), Some("2026-07-30"));
    }

    #[test]
    fn moves_lurkers_between_rooms() {
        let fixture = Fixture::new();
        fixture.run("untrusted bob grace=3d silent=180d");
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
        assert_eq!(fixture.silent("bob").as_deref(), Some("2026-05-06"));
    }

    #[test]
    fn needs_a_grace_period_to_mark_someone_new() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("trusted alice silent=1d"),
            [
                "Give a grace period too, such as: trusted alice grace=3d. It is how long after \
                 a warning I kick them from every channel and sajoin them into their room, or \
                 never."
            ]
        );
        assert_eq!(fixture.db.lurker("alice").unwrap(), None);
    }

    #[test]
    fn mistakes_change_nothing() {
        let fixture = Fixture::new();
        for (command, reply) in [
            (
                "untrusted bob grace=3d activity=daily year=2022",
                "activity and year are only for trusted lurkers.",
            ),
            (
                "untrusted bob grace=3d year=2022",
                "activity and year are only for trusted lurkers.",
            ),
            (
                "trusted bob grace=soon",
                "soon isn't a grace period. Give one in minutes, hours, days or weeks, such as \
                 90m, 12h, 3d or 2w, or 0 or never",
            ),
            (
                "trusted bob grace=3d activity=daily",
                "Give activity and year together, such as activity=daily year=2022.",
            ),
            (
                "trusted bob grace=3d year=2022",
                "Give activity and year together, such as activity=daily year=2022.",
            ),
            ("trusted bob grace=3d grace=1d", "grace is given twice."),
            (
                "trusted bob grace=3d silent=1d SILENT=2d",
                "silent is given twice.",
            ),
            (
                "trusted bob grace=3d color=blue",
                "I don't know the field color. Fields are grace, silent, activity and year.",
            ),
            (
                "trusted bob grace=3d #wetfish=180d",
                "I don't know the field #wetfish. Fields are grace, silent, activity and year.",
            ),
            (
                "trusted bob grace=3d since=180d",
                "I don't know the field since. Fields are grace, silent, activity and year.",
            ),
            (
                "trusted bob grace=3d peak=2022/daily",
                "I don't know the field peak. Fields are grace, silent, activity and year.",
            ),
            (
                "trusted bob grace=3d 180d",
                "180d isn't a field. Fields look like grace=3d, silent=180d, activity=daily and \
                 year=2022.",
            ),
            (
                "trusted bob grace=3d silent=2027-01-01",
                "2027-01-01 is in the future",
            ),
            (
                "trusted bob grace=3d activity=daily year=2030",
                "Give year as a number from 1988 to 2026.",
            ),
            (
                "trusted bob grace=3d activity=daily year=twenty",
                "Give year as a number from 1988 to 2026.",
            ),
            (
                "trusted bob grace=3d activity=hourly year=2022",
                "Give activity as daily, weekly or monthly.",
            ),
            (
                "trusted",
                "Usage: trusted <nick> grace=<time> [silent=<date>] \
                 [activity=<daily|weekly|monthly> year=<year>]",
            ),
            (
                "untrusted",
                "Usage: untrusted <nick> grace=<time> [silent=<date>]",
            ),
            ("trusted #wetfish grace=3d", "#wetfish isn't a nick."),
            ("trusted grace=3d", "grace=3d isn't a nick."),
        ] {
            assert_eq!(fixture.run(command), [reply], "{command}");
        }
        assert_eq!(fixture.db.lurker("bob").unwrap(), None);
    }

    #[test]
    fn shows_info() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");
        fixture.run("trusted alice grace=3d silent=348d activity=weekly year=2022");

        assert_eq!(
            fixture.run("info ALICE"),
            [
                "alice: trusted (#purgatory), in the room now, 0 days since first seen there. \
                 Marked by rachel on 2026-11-02.",
                "Silent since 2025-11-19 (348 days).",
                "Next ping: being scheduled.",
                "Kicks: warned the next time I find them online, and kicked 3 days later.",
                "Most active: chatted every week in 2022.",
                "As one command: trusted alice grace=3d silent=2025-11-19 activity=weekly \
                 year=2022",
            ]
        );

        fixture.run("trusted carol grace=1d");
        let info = fixture.run("info carol");
        assert_eq!(
            info[1],
            "Silent since: unknown. Add it with: trusted carol silent=<date>"
        );
        assert_eq!(
            info[4],
            "Most active: unknown. Add it with: trusted carol activity=<daily|weekly|monthly> \
             year=<year>"
        );

        fixture.run("untrusted bob grace=never");
        assert_eq!(
            fixture.run("info bob"),
            [
                "bob: untrusted (#FBI_watchlist), not in the room now, not seen there yet. \
                 Marked by rachel on 2026-11-02.",
                "Silent since: unknown. Add it with: untrusted bob silent=<date>",
                "Next ping: after they first join #FBI_watchlist.",
                "Kicks: never warned or kicked.",
                "As one command: untrusted bob grace=never",
            ]
        );
    }

    #[test]
    fn the_info_command_marks_the_same_lurker() {
        let fixture = Fixture::new();
        for (nick, command) in [
            (
                "alice",
                "trusted alice grace=90m silent=348d activity=daily year=2022",
            ),
            ("bob", "untrusted bob grace=2w silent=2026-05-06"),
        ] {
            fixture.run(command);
            let info = fixture.run(&format!("info {nick}"));
            let command = info
                .last()
                .unwrap()
                .strip_prefix("As one command: ")
                .unwrap();
            let before = fixture.lurker(nick);

            fixture.run(&format!("forget {nick}"));
            fixture.run(command);
            let after = fixture.lurker(nick);
            assert_eq!(
                (after.silent, after.grace, after.peak, after.room),
                (before.silent, before.grace, before.peak, before.room)
            );
        }
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
            "Kicks: warned on 2026-11-01, kicked from every channel in 2 days."
        );

        fixture.db.set_warned(id, NOW - 4 * DAY).unwrap();
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: warned on 2026-10-29, kicked from every channel and sajoined into \
             #FBI_watchlist whenever they're online."
        );

        fixture.db.set_spoke(id, NOW - DAY).unwrap();
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: stopped, since they spoke on 2026-11-01 after being warned. A new grace \
             period starts them again."
        );

        // Giving a grace period again starts over
        fixture.run("untrusted bob grace=1h");
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: warned the next time I find them online, and kicked 1 hour later."
        );
    }

    #[test]
    fn kicks_without_a_warning_with_no_grace_period() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("untrusted bob grace=0"),
            [
                "bob is marked untrusted (#FBI_watchlist). Kicked from every channel without a \
                 warning. Their time in #FBI_watchlist starts counting when they join it."
            ]
        );
        assert_eq!(fixture.lurker("bob").grace, Some(0));
        let info = fixture.run("info bob");
        assert_eq!(
            info[3],
            "Kicks: kicked without a warning the next time I find them online."
        );
        assert_eq!(
            info.last().unwrap(),
            "As one command: untrusted bob grace=0"
        );

        let id = fixture.lurker("bob").id;
        fixture.db.set_warned(id, NOW - DAY).unwrap();
        assert_eq!(
            fixture.run("info bob")[3],
            "Kicks: since 2026-11-01, without a warning, kicked from every channel and \
             sajoined into #FBI_watchlist whenever they're online."
        );
        fixture.db.set_spoke(id, NOW - 60).unwrap();
        assert_eq!(
            fixture.run("info bob")[3],
            "Kicks: stopped, since they spoke on 2026-11-02 after the kicks started. A new \
             grace period starts them again."
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
            "unchannel bob #wetfish",
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
    fn knows_its_commands() {
        for word in ["trusted", "LIST", "Poke", "channels", "help", "Nudge"] {
            assert!(is_command(word), "{word}");
        }
        for word in ["keeps", "unchannel", "", "lurker"] {
            assert!(!is_command(word), "{word}");
        }
        // Every command help lists is known
        let fixture = Fixture::new();
        for line in fixture.run("help") {
            let first = line.split([' ', ':']).next().unwrap();
            if line.contains(": ") && !line.starts_with("Fields") && !line.starts_with("silent") {
                assert!(is_command(first), "{line}");
            }
        }
    }

    #[test]
    fn keeps_the_nudge_list() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("nudge user=alice for=3w reminders=5"),
            [
                "alice is on the nudge list: 5 reminders over 21 days, about one every 4 days, \
                 until 2026-11-23."
            ]
        );
        assert_eq!(
            fixture.run("NUDGE FOR=1d user=bob REMINDERS=1"),
            ["bob is on the nudge list: 1 reminder over 1 day, until 2026-11-03."]
        );
        assert_eq!(
            fixture.run("nudge list"),
            [
                "Nudging (2): alice (0 of 5 reminders, until 2026-11-23, added by rachel), bob \
                 (0 of 1 reminder, until 2026-11-03, added by rachel)"
            ]
        );
        assert_eq!(
            fixture.run("nudge user=Alice for=1w reminders=2"),
            [
                "alice is already being nudged: 0 of 5 reminders, until 2026-11-23. Send nudge \
                 remove user=alice first to start again."
            ]
        );

        assert_eq!(
            fixture.run("nudge remove user=ALICE"),
            ["Stopped nudging alice after 0 of 5 reminders."]
        );
        assert_eq!(
            fixture.run("nudge remove user=alice"),
            ["alice isn't being nudged."]
        );
        assert_eq!(
            fixture.run("nudge list"),
            [
                "Nudging (1): bob (0 of 1 reminder, until 2026-11-03, added by rachel)",
                "Ended: alice, removed by rachel after 0 of 5 reminders (2026-11-02)",
            ]
        );

        // Nudged again after being removed
        fixture.run("nudge user=alice for=1w reminders=2");
        assert_eq!(fixture.run("nudge list")[0].matches("alice").count(), 1);
    }

    #[test]
    fn marking_takes_people_off_the_nudge_list() {
        let fixture = Fixture::new();
        fixture.run("nudge user=alice for=3w reminders=5");
        assert_eq!(
            fixture.run("trusted alice grace=3d"),
            [
                "alice is marked trusted (#purgatory). Kicked from every channel 3 days after \
                 being warned. Their time in #purgatory starts counting when they join it. \
                 Removed from the nudge list."
            ]
        );
        assert_eq!(
            fixture.run("nudge list"),
            [
                "Nudging (0): nobody",
                "Ended: alice, marked as a lurker by rachel (2026-11-02)",
            ]
        );
        assert_eq!(
            fixture.run("nudge user=alice for=3w reminders=5"),
            [
                "alice is marked trusted (#purgatory), and marked lurkers aren't nudged. Send \
                 forget alice first to nudge them instead."
            ]
        );
    }

    #[test]
    fn nudge_mistakes_change_nothing() {
        let fixture = Fixture::new();
        for (command, reply) in [
            ("nudge", NUDGE_USAGE.to_owned()),
            ("nudge user=alice", NUDGE_USAGE.to_owned()),
            ("nudge user=alice for=3w", NUDGE_USAGE.to_owned()),
            (
                "nudge user=alice for=soon reminders=5",
                "soon isn't a length of time. Give one in minutes, hours, days or weeks, such as \
                 90m, 12h, 3d or 2w"
                    .to_owned(),
            ),
            (
                "nudge user=alice for=30m reminders=1",
                "for must be from 1 hour to 365 days.".to_owned(),
            ),
            (
                "nudge user=alice for=3w reminders=0",
                "Give reminders as a number from 1 to 20.".to_owned(),
            ),
            (
                "nudge user=alice for=3w reminders=five",
                "Give reminders as a number from 1 to 20.".to_owned(),
            ),
            (
                "nudge user=alice for=3h reminders=5",
                "Reminders must be at least an hour apart. Give fewer reminders or a longer \
                 for=."
                    .to_owned(),
            ),
            (
                "nudge user=alice for=3w reminders=5 user=bob",
                "user is given twice.".to_owned(),
            ),
            (
                "nudge user=alice for=3w reminders=5 grace=3d",
                format!("I don't know the field grace. {NUDGE_USAGE}"),
            ),
            (
                "nudge alice for=3w reminders=5",
                format!("alice isn't a field. {NUDGE_USAGE}"),
            ),
            (
                "nudge user=#wetfish for=3w reminders=5",
                "#wetfish isn't a nick.".to_owned(),
            ),
            ("nudge remove", "Usage: nudge remove user=<nick>".to_owned()),
            (
                "nudge remove user=alice for=3w",
                format!("I don't know the field for. {NUDGE_USAGE}"),
            ),
        ] {
            assert_eq!(fixture.run(command), [reply], "{command}");
        }
        assert!(fixture.db.nudges().unwrap().is_empty());
    }

    #[test]
    fn says_how_ended_nudges_went() {
        let mut nudge = Nudge {
            id: 1,
            nick: "carol".to_owned(),
            length: 21 * DAY,
            reminders: 5,
            added_by: "rachel".to_owned(),
            added_at: NOW,
            sent: 2,
            first_reminder_at: Some(NOW + DAY),
            last_reminder_at: Some(NOW + DAY),
            spoke_at: Some(NOW + 3 * DAY),
            ended_at: Some(NOW + 3 * DAY),
            ended: Some(NudgeEnd::Spoke),
            ended_by: None,
        };
        assert_eq!(
            outcome(&nudge),
            "carol spoke 2 days after the first reminder (2026-11-05)"
        );
        nudge.ended = Some(NudgeEnd::Finished);
        assert_eq!(
            outcome(&nudge),
            "carol, no reply after 2 reminders (2026-11-05)"
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
