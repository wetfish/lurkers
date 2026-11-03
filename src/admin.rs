//! The commands admins send lurker, in private messages or in a channel
//! after its nick, to mark lurkers and record what they know about them.
//! `reload`, `poke`, `join`, `part` and `channels` are handled by the
//! connection, which holds the settings and knows the channels.

use chrono::Datelike;

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

impl From<rusqlite::Error> for Problem {
    fn from(err: rusqlite::Error) -> Problem {
        Problem::Database(err)
    }
}

type Reply = Result<Vec<String>, Problem>;

/// Carries out a command and returns the lines to reply with.
pub fn run(context: &Context, text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some((command, args)) = words.split_first() else {
        return help(context.rooms);
    };

    let reply = match command.to_ascii_lowercase().as_str() {
        "trusted" => mark(context, args, Room::Purgatory),
        "untrusted" => mark(context, args, Room::Watchlist),
        "grace" => grace(context, args),
        "channel" => channel(context, args),
        "unchannel" => unchannel(context, args),
        "peak" => peak(context, args),
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
                "Something went wrong with the database, so nothing changed. lurker's log has \
                 the details."
                    .to_owned(),
            ]
        }
    }
}

pub fn help(rooms: &Rooms) -> Vec<String> {
    let purgatory = rooms.channel(Room::Purgatory);
    let watchlist = rooms.channel(Room::Watchlist);
    vec![
        format!("trusted <nick> <grace>: mark a lurker for {purgatory}"),
        format!("untrusted <nick> <grace>: mark a lurker for {watchlist}"),
        "grace <nick> <grace>: how long after being warned a lurker is kicked from my channels, \
         such as 90m, 12h, 3d or 2w, or never"
            .to_owned(),
        "channel <nick> <#channel> <date>: a channel they idled in, with when they last spoke \
         there (trusted) or were first seen there (untrusted), as YYYY-MM-DD or a number of \
         days ago such as 348d"
            .to_owned(),
        "unchannel <nick> <#channel>: remove a channel".to_owned(),
        "peak <nick> <year> <daily|weekly|monthly>: how often a trusted lurker chatted in the \
         year they were most active"
            .to_owned(),
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
    let (nick, grace) = match args {
        [nick] => (*nick, None),
        [nick, grace] => (*nick, Some(dates::parse_grace(grace)?)),
        _ => return Err(format!("Usage: {trust} <nick> <grace>").into()),
    };
    let nick = check_nick(nick)?;
    if grace.is_none() && context.db.lurker(nick)?.is_none() {
        return Err(format!(
            "Give a grace period too, such as: {trust} {nick} 3d. It is how long after being \
             warned they are kicked from my channels, or never."
        )
        .into());
    }

    let channel = context.rooms.channel(room);
    let here = context.presence.contains(channel, nick);
    let marked = context
        .db
        .mark(nick, room, here, context.admin, context.now)?;
    let grace_sentence = match grace {
        Some(grace) => {
            let id = find(context, nick)?.id;
            context.db.set_grace(id, grace)?;
            format!(" {}.", capitalize(&kicked(grace)))
        }
        None => String::new(),
    };

    let mut reply = match marked {
        Marked::New => format!("{nick} is marked {trust} ({channel})."),
        Marked::Moved => format!(
            "{nick} is marked {trust} ({channel}) instead of {} ({}).",
            self::trust(other(room)),
            context.rooms.channel(other(room))
        ),
        Marked::Unchanged => {
            return Ok(vec![format!(
                "{nick} is already marked {trust} ({channel}).{grace_sentence}"
            )]);
        }
    };
    reply.push_str(&grace_sentence);
    if !here {
        reply.push_str(&format!(
            " Their time in {channel} starts counting when they join it."
        ));
    }
    Ok(vec![reply])
}

fn grace(context: &Context, args: &[&str]) -> Reply {
    let [nick, grace] = args else {
        return Err(
            "Usage: grace <nick> <grace>, where grace is such as 90m, 12h, 3d or 2w, \
                    or never"
                .to_owned()
                .into(),
        );
    };
    let lurker = find(context, nick)?;
    let grace = dates::parse_grace(grace)?;
    context.db.set_grace(lurker.id, grace)?;
    Ok(vec![format!("{}: {}.", lurker.nick, kicked(grace))])
}

fn channel(context: &Context, args: &[&str]) -> Reply {
    let [nick, channel, date] = args else {
        return Err("Usage: channel <nick> <#channel> <YYYY-MM-DD or 348d>"
            .to_owned()
            .into());
    };
    let lurker = find(context, nick)?;
    let channel = check_channel(channel)?;
    let today = dates::today(context.now);
    let since = dates::parse(date, today)?;

    context.db.set_channel(lurker.id, channel, since)?;
    Ok(vec![format!(
        "{}: {channel}, {} {since} ({}).",
        lurker.nick,
        since_word(lurker.room),
        dates::days(dates::days_since(since, today))
    )])
}

fn unchannel(context: &Context, args: &[&str]) -> Reply {
    let [nick, channel] = args else {
        return Err("Usage: unchannel <nick> <#channel>".to_owned().into());
    };
    let lurker = find(context, nick)?;
    let channel = check_channel(channel)?;

    if context.db.remove_channel(lurker.id, channel)? {
        Ok(vec![format!("{}: removed {channel}.", lurker.nick)])
    } else {
        Ok(vec![format!("{} has no {channel}.", lurker.nick)])
    }
}

fn peak(context: &Context, args: &[&str]) -> Reply {
    let [nick, year, frequency] = args else {
        return Err("Usage: peak <nick> <year> <daily|weekly|monthly>"
            .to_owned()
            .into());
    };
    let lurker = find(context, nick)?;
    if lurker.room != Room::Purgatory {
        return Err(format!(
            "peak is only for trusted lurkers, and {} is untrusted.",
            lurker.nick
        )
        .into());
    }

    let this_year = dates::today(context.now).year();
    let year: i32 = year
        .parse()
        .ok()
        .filter(|year| (1988..=this_year).contains(year))
        .ok_or_else(|| format!("Give the year as a number from 1988 to {this_year}."))?;
    let frequency = Frequency::parse(frequency)
        .ok_or_else(|| "Give how often as daily, weekly or monthly.".to_owned())?;

    context.db.set_peak(lurker.id, year, frequency)?;
    Ok(vec![format!(
        "{}: chatted {} in {year}.",
        lurker.nick,
        frequency.phrase()
    )])
}

fn info(context: &Context, args: &[&str]) -> Reply {
    let [nick] = args else {
        return Err("Usage: info <nick>".to_owned().into());
    };
    let lurker = find(context, nick)?;
    let today = dates::today(context.now);
    let channel = context.rooms.channel(lurker.room);
    let name = &lurker.nick;

    let here = context.presence.contains(channel, name);
    let time = match lurker.here_since {
        Some(since) => format!(
            "{} since first seen there",
            dates::days((context.now - since).max(0) / 86_400)
        ),
        None => "not seen there yet".to_owned(),
    };
    let mut lines = vec![format!(
        "{name}: {} ({channel}), {}, {time}. Marked by {} on {}.",
        trust(lurker.room),
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
            "No channels yet. Add them with: channel {name} <#channel> <date>"
        ));
    } else {
        let channels = lurker
            .channels
            .iter()
            .map(|idle| {
                format!(
                    "{} {} {} ({})",
                    idle.channel,
                    since_word(lurker.room),
                    idle.since,
                    dates::days(dates::days_since(idle.since, today))
                )
            })
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
                "Most active: unknown. Add it with: peak {name} <year> <daily|weekly|monthly>"
            ),
        });
    }
    Ok(lines)
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
        return Err("Usage: forget <nick>".to_owned().into());
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
            "{nick} isn't marked. Mark them first with \"trusted {nick}\" or \"untrusted {nick}\"."
        ))
    })
}

fn check_nick(nick: &str) -> Result<&str, Problem> {
    if nick.starts_with(['#', '&']) || nick.contains([',', '!', '@', '*']) {
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

/// What a channel's date means for a lurker in a room.
fn since_word(room: Room) -> &'static str {
    match room {
        Room::Purgatory => "last spoke",
        Room::Watchlist => "first seen",
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
    }

    #[test]
    fn marks_lurkers() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");

        assert_eq!(
            fixture.run("trusted alice 3d"),
            [
                "alice is marked trusted (#purgatory). Kicked from my channels 3 days after being \
              warned."
            ]
        );
        assert_eq!(
            fixture.run("UNTRUSTED bob never"),
            [
                "bob is marked untrusted (#FBI_watchlist). Never kicked from my channels. Their \
                 time in #FBI_watchlist starts counting when they join it."
            ]
        );
        assert_eq!(
            fixture.run("trusted Alice"),
            ["Alice is already marked trusted (#purgatory)."]
        );
        assert_eq!(
            fixture.run("trusted bob"),
            [
                "bob is marked trusted (#purgatory) instead of untrusted (#FBI_watchlist). Their \
                 time in #purgatory starts counting when they join it."
            ]
        );

        let alice = fixture.db.lurker("alice").unwrap().unwrap();
        assert_eq!(alice.here_since, Some(NOW));
        assert_eq!(alice.marked_by, "rachel");
        assert_eq!(alice.grace, Some(3 * DAY));
        assert_eq!(fixture.db.lurker("bob").unwrap().unwrap().grace, None);
    }

    #[test]
    fn needs_a_grace_period_to_mark_someone_new() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("trusted alice"),
            [
                "Give a grace period too, such as: trusted alice 3d. It is how long after being \
                 warned they are kicked from my channels, or never."
            ]
        );
        assert_eq!(fixture.db.lurker("alice").unwrap(), None);
        assert_eq!(
            fixture.run("trusted alice soon"),
            [
                "soon isn't a grace period. Give one in minutes, hours, days or weeks, such as \
                 90m, 12h, 3d or 2w, or never"
            ]
        );
        assert_eq!(fixture.run("trusted"), ["Usage: trusted <nick> <grace>"]);
    }

    #[test]
    fn changes_grace_periods() {
        let fixture = Fixture::new();
        fixture.run("untrusted bob 3d");

        assert_eq!(
            fixture.run("grace bob 12h"),
            ["bob: kicked from my channels 12 hours after being warned."]
        );
        assert_eq!(
            fixture.run("untrusted bob 90m"),
            [
                "bob is already marked untrusted (#FBI_watchlist). Kicked from my channels 90 \
              minutes after being warned."
            ]
        );
        assert_eq!(
            fixture.run("grace bob never"),
            ["bob: never kicked from my channels."]
        );
        assert_eq!(fixture.db.lurker("bob").unwrap().unwrap().grace, None);
    }

    #[test]
    fn records_channels() {
        let fixture = Fixture::new();
        fixture.run("trusted alice 3d");
        fixture.run("untrusted bob 3d");

        assert_eq!(
            fixture.run("channel alice #wetfish 2024-11-19"),
            ["alice: #wetfish, last spoke 2024-11-19 (713 days)."]
        );
        assert_eq!(
            fixture.run("channel bob #wetfish 180d"),
            ["bob: #wetfish, first seen 2026-05-06 (180 days)."]
        );
        assert_eq!(
            fixture.run("channel alice #wetfish 2027-01-01"),
            ["2027-01-01 is in the future"]
        );
        assert_eq!(
            fixture.run("channel alice wetfish 1d"),
            ["wetfish isn't a channel. Channels start with #."]
        );
        assert_eq!(
            fixture.run("unchannel alice #WETFISH"),
            ["alice: removed #WETFISH."]
        );
        assert_eq!(
            fixture.run("unchannel alice #wetfish"),
            ["alice has no #wetfish."]
        );
    }

    #[test]
    fn records_peak_activity_for_trusted_lurkers() {
        let fixture = Fixture::new();
        fixture.run("trusted alice 3d");
        fixture.run("untrusted bob 3d");

        assert_eq!(
            fixture.run("peak alice 2022 Daily"),
            ["alice: chatted every day in 2022."]
        );
        assert_eq!(
            fixture.run("peak bob 2022 daily"),
            ["peak is only for trusted lurkers, and bob is untrusted."]
        );
        assert_eq!(
            fixture.run("peak alice 2030 daily"),
            ["Give the year as a number from 1988 to 2026."]
        );
        assert_eq!(
            fixture.run("peak alice 2022 hourly"),
            ["Give how often as daily, weekly or monthly."]
        );
    }

    #[test]
    fn shows_info() {
        let mut fixture = Fixture::new();
        fixture.presence.add("#purgatory", "alice");
        fixture.run("trusted alice 3d");
        fixture.run("channel alice #wetfish 348d");
        fixture.run("channel alice #dryfish 2026-11-01");
        fixture.run("peak alice 2022 weekly");

        assert_eq!(
            fixture.run("info ALICE"),
            [
                "alice: trusted (#purgatory), in the room now, 0 days since first seen there. \
                 Marked by rachel on 2026-11-02.",
                "Channels: #dryfish last spoke 2026-11-01 (1 day), #wetfish last spoke \
                 2025-11-19 (348 days)",
                "Next ping: being scheduled.",
                "Kicks: warned when I see them in a channel where I can kick, and kicked 3 days \
                 later.",
                "Most active: chatted every week in 2022.",
            ]
        );

        fixture.run("untrusted bob never");
        assert_eq!(
            fixture.run("info bob"),
            [
                "bob: untrusted (#FBI_watchlist), not in the room now, not seen there yet. \
                 Marked by rachel on 2026-11-02.",
                "No channels yet. Add them with: channel bob <#channel> <date>",
                "Next ping: after they first join #FBI_watchlist.",
                "Kicks: never kicked from my channels.",
            ]
        );
    }

    #[test]
    fn shows_when_the_next_ping_comes() {
        let fixture = Fixture::new();
        fixture.run("trusted alice 3d");
        let id = fixture.db.lurker("alice").unwrap().unwrap().id;

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
        fixture.run("untrusted bob 3d");
        let id = fixture.db.lurker("bob").unwrap().unwrap().id;

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
        fixture.run("trusted carol 3d");
        fixture.run("trusted alice 3d");

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
        fixture.run("trusted alice 3d");
        assert_eq!(fixture.run("forget alice"), ["Forgot alice."]);
        assert_eq!(fixture.run("forget alice"), ["alice isn't marked."]);
    }

    #[test]
    fn explains_mistakes() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run("info nobody"),
            [
                "nobody isn't marked. Mark them first with \"trusted nobody\" or \"untrusted \
                 nobody\"."
            ]
        );
        assert_eq!(
            fixture.run("trusted #wetfish 3d"),
            ["#wetfish isn't a nick."]
        );
        assert_eq!(
            fixture.run("dance"),
            ["I don't know the command dance. Send \"help\" for the list."]
        );
        assert_eq!(fixture.run("help").len(), 14);
        assert_eq!(fixture.run("").len(), 14);
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
