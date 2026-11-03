//! What lurker says in its rooms: the templates in `messages.toml`, which
//! are built into lurker, filled in with what is recorded about a lurker.
//! Also the notice admins get when a lurker speaks.

use chrono::NaiveDate;
use serde::Deserialize;

use crate::config::Room;
use crate::db::{IdleChannel, Lurker};
use crate::rng::Rng;
use crate::units::{self, Unit};

const BUILTIN: &str = include_str!("../messages.toml");

/// How much of a lurker's message admins are shown, in characters.
const QUOTE_LENGTH: usize = 200;

/// The kinds of message in each room.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Ping,
    Welcome,
    Fanfare,
}

pub struct Messages {
    purgatory: Set,
    watchlist: Set,
}

/// One room's messages.
struct Set {
    ping: Vec<Template>,
    welcome: Vec<Template>,
    fanfare: Vec<Template>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    purgatory: FileSet,
    watchlist: FileSet,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSet {
    ping: Vec<String>,
    welcome: Vec<String>,
    fanfare: Vec<String>,
}

struct Template {
    parts: Vec<Part>,
}

enum Part {
    Text(String),
    Field(Field, Measure),
}

/// What a placeholder stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Field {
    Nick,
    /// Time since they first joined their room
    Here,
    Channel,
    Channels,
    /// Time since they last spoke in any of their channels
    Idle,
    /// Time since they last spoke in `Channel`
    ChannelIdle,
    LastSpoke,
    PeakYear,
    PeakFrequency,
    /// Time since they were first seen in any of their channels
    Lurking,
    /// Time since they were first seen in `Channel`
    ChannelLurking,
    FirstSeen,
}

/// The unit a time is said in.
#[derive(Clone, Copy)]
enum Measure {
    In(&'static Unit),
    /// A unit picked at random each time
    Silly,
}

impl Messages {
    /// The messages lurker is built with.
    pub fn builtin() -> Result<Messages, String> {
        Messages::parse(BUILTIN).map_err(|err| format!("messages.toml: {err}"))
    }

    fn parse(text: &str) -> Result<Messages, String> {
        let file: File = toml::from_str(text).map_err(|err| err.to_string())?;
        Ok(Messages {
            purgatory: Set::parse(file.purgatory, Room::Purgatory)?,
            watchlist: Set::parse(file.watchlist, Room::Watchlist)?,
        })
    }

    fn templates(&self, room: Room, kind: Kind) -> &[Template] {
        let set = match room {
            Room::Purgatory => &self.purgatory,
            Room::Watchlist => &self.watchlist,
        };
        match kind {
            Kind::Ping => &set.ping,
            Kind::Welcome => &set.welcome,
            Kind::Fanfare => &set.fanfare,
        }
    }

    /// Picks a message for a lurker in a room, among the templates that
    /// everything they need is recorded for. Templates in `used` are only
    /// picked once every other one has been, so a lurker hears them all
    /// before any repeats. Returns the template's number and the message.
    pub fn choose(
        &self,
        room: Room,
        kind: Kind,
        lurker: &Lurker,
        now: i64,
        rng: &mut Rng,
        used: &[usize],
    ) -> Option<(usize, String)> {
        let rendered: Vec<(usize, String)> = self
            .templates(room, kind)
            .iter()
            .enumerate()
            .filter_map(|(index, template)| {
                template
                    .render(room, lurker, now, rng)
                    .map(|text| (index, text))
            })
            .collect();
        let fresh: Vec<(usize, String)> = rendered
            .iter()
            .filter(|(index, _)| !used.contains(index))
            .cloned()
            .collect();

        let pool = if fresh.is_empty() { &rendered } else { &fresh };
        rng.pick(pool).cloned()
    }
}

impl Set {
    fn parse(file: FileSet, room: Room) -> Result<Set, String> {
        let name = room.as_str();
        let list = |kind: &str, texts: Vec<String>| -> Result<Vec<Template>, String> {
            if texts.is_empty() {
                return Err(format!("{name}.{kind} needs at least one message"));
            }
            texts
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    Template::parse(text, room)
                        .map_err(|err| format!("{name}.{kind}, message {}: {err}", index + 1))
                })
                .collect()
        };
        Ok(Set {
            ping: list("ping", file.ping)?,
            welcome: list("welcome", file.welcome)?,
            fanfare: list("fanfare", file.fanfare)?,
        })
    }
}

impl Field {
    /// The field a placeholder names, if the room's messages can use it.
    fn parse(name: &str, room: Room) -> Option<Field> {
        let field = match name {
            "nick" => Field::Nick,
            "here" => Field::Here,
            "channel" => Field::Channel,
            "channels" => Field::Channels,
            "idle" => Field::Idle,
            "channel_idle" => Field::ChannelIdle,
            "last_spoke" => Field::LastSpoke,
            "peak_year" => Field::PeakYear,
            "peak_frequency" => Field::PeakFrequency,
            "lurking" => Field::Lurking,
            "channel_lurking" => Field::ChannelLurking,
            "first_seen" => Field::FirstSeen,
            _ => return None,
        };
        let purgatory_only = matches!(
            field,
            Field::Idle
                | Field::ChannelIdle
                | Field::LastSpoke
                | Field::PeakYear
                | Field::PeakFrequency
        );
        let watchlist_only = matches!(
            field,
            Field::Lurking | Field::ChannelLurking | Field::FirstSeen
        );
        match room {
            Room::Purgatory if watchlist_only => None,
            Room::Watchlist if purgatory_only => None,
            _ => Some(field),
        }
    }

    fn is_time(self) -> bool {
        matches!(
            self,
            Field::Here | Field::Idle | Field::ChannelIdle | Field::Lurking | Field::ChannelLurking
        )
    }
}

impl Template {
    fn parse(text: &str, room: Room) -> Result<Template, String> {
        let mut parts = Vec::new();
        let mut rest = text;

        while let Some(open) = rest.find('{') {
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_owned()));
            }
            let after = &rest[open + 1..];
            let close = after
                .find('}')
                .ok_or_else(|| "a { has no } after it".to_owned())?;
            let inside = &after[..close];
            let (name, unit) = match inside.split_once(':') {
                Some((name, unit)) => (name, Some(unit)),
                None => (inside, None),
            };

            let field = Field::parse(name, room).ok_or_else(|| {
                format!(
                    "{{{name}}} isn't a placeholder {} messages can use",
                    room.as_str()
                )
            })?;
            let measure = match unit {
                None => Measure::In(units::days()),
                Some(_) if !field.is_time() => {
                    return Err(format!("{{{inside}}}: only times can name a unit"));
                }
                Some("silly") => Measure::Silly,
                Some(unit) => Measure::In(
                    units::find(unit)
                        .ok_or_else(|| format!("{{{inside}}}: there is no unit called {unit}"))?,
                ),
            };
            parts.push(Part::Field(field, measure));
            rest = &after[close + 1..];
        }

        if rest.contains('}') {
            return Err("a } has no { before it".to_owned());
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Template { parts })
    }

    /// The message for a lurker, or None when it needs something that isn't
    /// recorded about them.
    fn render(&self, room: Room, lurker: &Lurker, now: i64, rng: &mut Rng) -> Option<String> {
        let loud = room == Room::Watchlist;
        let channel = rng.pick(&lurker.channels);
        let newest = lurker.channels.iter().map(|idle| idle.since).max();
        let oldest = lurker.channels.iter().map(|idle| idle.since).min();
        let mut text = String::new();

        for part in &self.parts {
            let value = match part {
                Part::Text(literal) => literal.clone(),
                Part::Field(field, measure) => {
                    let mut time = |seconds: i64| shout(&duration(seconds, *measure, rng), loud);
                    match field {
                        Field::Nick => lurker.nick.clone(),
                        Field::Here => time(now - lurker.here_since?),
                        Field::Channel => channel?.channel.clone(),
                        Field::Channels => list(&lurker.channels, loud)?,
                        Field::Idle => time(seconds_since(newest?, now)),
                        Field::Lurking => time(seconds_since(oldest?, now)),
                        Field::ChannelIdle | Field::ChannelLurking => {
                            time(seconds_since(channel?.since, now))
                        }
                        Field::LastSpoke => shout(&date(newest?), loud),
                        Field::FirstSeen => shout(&date(oldest?), loud),
                        Field::PeakYear => lurker.peak?.0.to_string(),
                        Field::PeakFrequency => shout(lurker.peak?.1.phrase(), loud),
                    }
                }
            };
            text.push_str(&value);
        }
        Some(text)
    }
}

/// The notice admins get when a lurker speaks in their room.
pub fn notice(channel: &str, lurker: &Lurker, said: &str, now: i64) -> String {
    let days = units::days();
    let here = days.format(lurker.here_since.map_or(0, |since| now - since));
    let names: Vec<&str> = lurker
        .channels
        .iter()
        .map(|idle| idle.channel.as_str())
        .collect();
    let newest = lurker.channels.iter().map(|idle| idle.since).max();
    let oldest = lurker.channels.iter().map(|idle| idle.since).min();

    let details = match (lurker.room, newest, oldest) {
        (Room::Purgatory, Some(newest), _) => format!(
            "last spoke {} ago in {}",
            days.format(seconds_since(newest, now)),
            names.join(", ")
        ),
        (Room::Watchlist, _, Some(oldest)) => format!(
            "lurked {} in {}",
            days.format(seconds_since(oldest, now)),
            names.join(", ")
        ),
        _ => "no notes recorded".to_owned(),
    };

    format!(
        "{channel}: {} spoke after {here} in the room ({details}): \"{}\"",
        lurker.nick,
        quote(said)
    )
}

/// A lurker's message as admins are shown it: an action as `/me`, and cut
/// short when it is long.
fn quote(said: &str) -> String {
    let said = match said.strip_prefix("\u{1}ACTION") {
        Some(action) => format!("/me{}", action.trim_end_matches('\u{1}')),
        None => said.to_owned(),
    };
    if said.chars().count() <= QUOTE_LENGTH {
        said
    } else {
        let cut: String = said.chars().take(QUOTE_LENGTH).collect();
        format!("{cut}…")
    }
}

/// A length of time in a unit, picking a silly unit that comes to at least
/// 2, so messages never say "0 dog years" or "1 full moons".
fn duration(seconds: i64, measure: Measure, rng: &mut Rng) -> String {
    let unit = match measure {
        Measure::In(unit) => unit,
        Measure::Silly => {
            let fitting: Vec<&'static Unit> = units::UNITS
                .iter()
                .filter(|unit| unit.count(seconds) >= 2)
                .collect();
            rng.pick(&fitting).copied().unwrap_or(&units::UNITS[0])
        }
    };
    unit.format(seconds)
}

/// Seconds from the start of a day, in UTC, to now.
fn seconds_since(date: NaiveDate, now: i64) -> i64 {
    let start = date
        .and_hms_opt(0, 0, 0)
        .map_or(now, |midnight| midnight.and_utc().timestamp());
    (now - start).max(0)
}

/// A date as messages say it, such as "March 3, 2025".
fn date(date: NaiveDate) -> String {
    date.format("%B %-d, %Y").to_string()
}

/// Channels as messages list them, such as "#wetfish, #music and #dryfish".
fn list(channels: &[IdleChannel], loud: bool) -> Option<String> {
    let (last, rest) = channels.split_last()?;
    if rest.is_empty() {
        return Some(last.channel.clone());
    }
    let rest: Vec<&str> = rest.iter().map(|idle| idle.channel.as_str()).collect();
    let and = if loud { "AND" } else { "and" };
    Some(format!("{} {and} {}", rest.join(", "), last.channel))
}

/// Text in capitals for #FBI_watchlist.
fn shout(text: &str, loud: bool) -> String {
    if loud {
        text.to_uppercase()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Frequency;

    const DAY: i64 = 86_400;
    /// 2026-11-02 12:00 UTC
    const NOW: i64 = 1_793_620_800;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn lurker(room: Room) -> Lurker {
        Lurker {
            id: 1,
            nick: "Alice".to_owned(),
            room,
            channels: Vec::new(),
            peak: None,
            here_since: Some(NOW - 41 * DAY),
            marked_by: "rachel".to_owned(),
            marked_at: NOW - 41 * DAY,
            next_ping_at: None,
            fanfare_at: None,
        }
    }

    /// A lurker with everything recorded.
    fn noted(room: Room) -> Lurker {
        Lurker {
            channels: vec![
                IdleChannel {
                    channel: "#dryfish".to_owned(),
                    since: day("2025-11-19"),
                },
                IdleChannel {
                    channel: "#wetfish".to_owned(),
                    since: day("2024-11-19"),
                },
            ],
            peak: Some((2022, Frequency::Daily)),
            ..lurker(room)
        }
    }

    fn render(text: &str, room: Room, lurker: &Lurker) -> Option<String> {
        Template::parse(text, room)
            .unwrap()
            .render(room, lurker, NOW, &mut Rng::seeded(1))
    }

    #[test]
    fn the_builtin_messages_are_valid() {
        let messages = Messages::builtin().unwrap();
        assert_eq!(messages.purgatory.ping.len(), 22);
        assert_eq!(messages.purgatory.welcome.len(), 4);
        assert_eq!(messages.purgatory.fanfare.len(), 5);
        assert_eq!(messages.watchlist.ping.len(), 23);
        assert_eq!(messages.watchlist.welcome.len(), 4);
        assert_eq!(messages.watchlist.fanfare.len(), 5);
    }

    #[test]
    fn every_builtin_message_renders_for_a_lurker_with_notes() {
        let messages = Messages::builtin().unwrap();
        let mut rng = Rng::seeded(1);
        for room in Room::ALL {
            let lurker = noted(room);
            for kind in [Kind::Ping, Kind::Welcome, Kind::Fanfare] {
                for template in messages.templates(room, kind) {
                    for _ in 0..20 {
                        let text = template.render(room, &lurker, NOW, &mut rng).unwrap();
                        assert!(!text.contains(['{', '}']), "{text}");
                        assert!(text.contains("Alice"), "{text}");
                    }
                }
            }
        }
    }

    #[test]
    fn lurkers_without_notes_only_get_messages_that_need_none() {
        let messages = Messages::builtin().unwrap();
        let mut rng = Rng::seeded(1);
        let count = |room| {
            messages
                .templates(room, Kind::Ping)
                .iter()
                .filter(|template| {
                    template
                        .render(room, &lurker(room), NOW, &mut Rng::seeded(1))
                        .is_some()
                })
                .count()
        };
        assert_eq!(count(Room::Purgatory), 5);
        assert_eq!(count(Room::Watchlist), 9);

        let (_, text) = messages
            .choose(
                Room::Watchlist,
                Kind::Fanfare,
                &lurker(Room::Watchlist),
                NOW,
                &mut rng,
                &[],
            )
            .unwrap();
        assert!(!text.contains('{'), "{text}");
    }

    #[test]
    fn fills_in_purgatory_messages() {
        let alice = noted(Room::Purgatory);
        assert_eq!(
            render(
                "{nick}: Hey friend! I haven't seen you in {idle}.",
                Room::Purgatory,
                &alice
            )
            .unwrap(),
            "Alice: Hey friend! I haven't seen you in 348 days."
        );
        assert_eq!(
            render(
                "{nick}: Wow! You used to chat {peak_frequency} in {peak_year} but now you're gone 😢",
                Room::Purgatory,
                &alice
            )
            .unwrap(),
            "Alice: Wow! You used to chat every day in 2022 but now you're gone 😢"
        );
        assert_eq!(
            render(
                "{channels}; last {last_spoke}; here {here}; {idle:seconds}",
                Room::Purgatory,
                &alice
            )
            .unwrap(),
            "#dryfish and #wetfish; last November 19, 2025; here 41 days; 30,110,400 seconds"
        );
    }

    #[test]
    fn shouts_in_the_watchlist_but_keeps_names() {
        let bob = noted(Room::Watchlist);
        assert_eq!(
            render(
                "{nick}! YOU'VE BEEN HERE FOR {lurking} WITHOUT SAYING A WORD. WHAT'S YOUR PROBLEM?! MAGGOT",
                Room::Watchlist,
                &bob
            )
            .unwrap(),
            "Alice! YOU'VE BEEN HERE FOR 713 DAYS WITHOUT SAYING A WORD. WHAT'S YOUR PROBLEM?! MAGGOT"
        );
        assert_eq!(
            render(
                "{channels} {first_seen} {lurking:dog_years}",
                Room::Watchlist,
                &bob
            )
            .unwrap(),
            "#dryfish AND #wetfish NOVEMBER 19, 2024 13 DOG YEARS"
        );
    }

    #[test]
    fn a_channel_and_its_time_match() {
        let alice = noted(Room::Purgatory);
        for seed in 0..20 {
            let text = Template::parse("{channel} {channel_idle}", Room::Purgatory)
                .unwrap()
                .render(Room::Purgatory, &alice, NOW, &mut Rng::seeded(seed))
                .unwrap();
            assert!(
                text == "#dryfish 348 days" || text == "#wetfish 713 days",
                "{text}"
            );
        }
    }

    #[test]
    fn silly_units_come_to_at_least_two() {
        let alice = noted(Room::Purgatory);
        for seed in 0..50 {
            let text = Template::parse("{here:silly}", Room::Purgatory)
                .unwrap()
                .render(Room::Purgatory, &alice, NOW, &mut Rng::seeded(seed))
                .unwrap();
            let count: i64 = text
                .split(' ')
                .next()
                .unwrap()
                .replace(',', "")
                .parse()
                .unwrap();
            assert!(count >= 2, "{text}");
        }
    }

    #[test]
    fn skips_messages_missing_data() {
        let alice = lurker(Room::Purgatory);
        assert_eq!(render("{idle}", Room::Purgatory, &alice), None);
        assert_eq!(render("{peak_year}", Room::Purgatory, &alice), None);
        let unseen = Lurker {
            here_since: None,
            ..alice
        };
        assert_eq!(render("{here}", Room::Purgatory, &unseen), None);
    }

    #[test]
    fn rejects_bad_templates() {
        for (text, room) in [
            ("{nickname}", Room::Purgatory),
            ("{lurking}", Room::Purgatory),
            ("{idle}", Room::Watchlist),
            ("{nick:seconds}", Room::Purgatory),
            ("{here:fortnightly}", Room::Purgatory),
            ("{nick", Room::Purgatory),
            ("nick}", Room::Purgatory),
        ] {
            assert!(Template::parse(text, room).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn goes_through_every_ping_before_repeating() {
        let messages = Messages::builtin().unwrap();
        let alice = lurker(Room::Purgatory);
        let mut rng = Rng::seeded(3);
        let mut used = Vec::new();
        for _ in 0..5 {
            let (index, _) = messages
                .choose(Room::Purgatory, Kind::Ping, &alice, NOW, &mut rng, &used)
                .unwrap();
            assert!(!used.contains(&index));
            used.push(index);
        }
        // All five that need no notes are used, so one of them repeats
        let (index, _) = messages
            .choose(Room::Purgatory, Kind::Ping, &alice, NOW, &mut rng, &used)
            .unwrap();
        assert!(used.contains(&index));
    }

    #[test]
    fn tells_admins_what_a_lurker_said() {
        let alice = Lurker {
            channels: vec![
                IdleChannel {
                    channel: "#wetfish".to_owned(),
                    since: day("2025-11-19"),
                },
                IdleChannel {
                    channel: "#dryfish".to_owned(),
                    since: day("2024-11-19"),
                },
            ],
            ..lurker(Room::Purgatory)
        };
        assert_eq!(
            notice("#purgatory", &alice, "hello? is anyone here", NOW),
            "#purgatory: Alice spoke after 41 days in the room (last spoke 348 days ago in \
             #wetfish, #dryfish): \"hello? is anyone here\""
        );

        let bob = Lurker {
            channels: vec![IdleChannel {
                channel: "#wetfish".to_owned(),
                since: day("2026-05-06"),
            }],
            here_since: Some(NOW - 12 * DAY),
            ..lurker(Room::Watchlist)
        };
        assert_eq!(
            notice("#FBI_watchlist", &bob, "\u{1}ACTION waves\u{1}", NOW),
            "#FBI_watchlist: Alice spoke after 12 days in the room (lurked 180 days in \
             #wetfish): \"/me waves\""
        );

        let carol = lurker(Room::Purgatory);
        let long = "a".repeat(300);
        let text = notice("#purgatory", &carol, &long, NOW);
        assert!(text.contains("(no notes recorded)"), "{text}");
        assert!(text.ends_with("a…\""), "{text}");
    }
}
