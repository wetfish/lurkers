# Commands

Admins send commands to lurker in a private message, such as
`/msg lurker list`, or in any channel lurker is in after its nick, such as
`lurker: list`, `lurker, list` or `lurker list`. lurker answers in the same
place, so in a channel everyone there sees the answer. In a channel, lurker
only answers its commands: a message that starts with its nick but not a
command, such as `lurker keeps track of everyone`, gets no reply. In a
private message, it says when it doesn't know a command. Only the
[admins in the settings](configuration.md#admins) can use them, and lurker
logs every one.

| Command | Does |
| --- | --- |
| `trusted <nick> <fields>` | Marks or updates a lurker for #purgatory |
| `untrusted <nick> <fields>` | Marks or updates a lurker for #FBI_watchlist |
| `info <nick>` | Shows everything recorded about a lurker, ending with it all as one command |
| `list` | Lists the lurkers for each room, marking those in it now with `(here)` |
| `forget <nick>` | Deletes a lurker |
| `nudge user=<nick> for=<time> reminders=<number>` | Puts someone on the [nudge list](#nudges) |
| `nudge list` | Lists who is being nudged, and how the nudges that ended went |
| `nudge remove user=<nick>` | Takes someone off the nudge list |
| `poke <nick>` | Sends a lurker in their room their weekly ping now, and schedules the next one a week later |
| `join <#channel>` | Joins a channel and rejoins it after restarts, the same as inviting lurker there |
| `part <#channel>` | Leaves a channel for good |
| `channels` | Lists lurker's channels besides its rooms, and whether it can kick in each |
| `reload` | Reads the settings file again |
| `help` | Lists the commands |

## Marking lurkers

`trusted` and `untrusted` take the nick, then any of these fields, in any
order:

| Field | Means |
| --- | --- |
| `grace=<time>` | How long after a [warning](how-it-works.md#warnings-and-kicks) lurker kicks them from every channel and moves them to their room: such as `90m`, `12h`, `3d` or `2w`, `0` to kick them without a warning, or `never`. Giving it starts their warning over. |
| `silent=<date>` | When they went silent: when they last spoke anywhere, or showed up if they never did |
| `activity=<daily\|weekly\|monthly>` | How often a trusted lurker chatted in the year they were most active. Given together with `year`. |
| `year=<year>` | The year a trusted lurker was most active. Given together with `activity`. |

```text
untrusted bob grace=3d silent=180d
trusted alice grace=7d silent=348d activity=daily year=2024
trusted carol grace=2w silent=2025-11-19
```

Dates are `YYYY-MM-DD`, or a number of days ago such as `180d`, which lurker
turns into a date so the count keeps growing. Days are counted in UTC. The
numbers can come from `wot analyze --nick`.

- Someone new needs `grace`, so each lurker can be given their own.
- Marking someone again changes only the fields given, so
  `untrusted bob grace=never` only changes bob's grace period.
- A mistake anywhere in a command changes nothing.
- Marking someone for the other room moves them. Their time in the room,
  their pings and their warning start again; their grace period and silent
  date stay.

`info` ends with everything recorded as one command, such as
`As one command: untrusted bob grace=3d silent=2026-05-06`, to copy, change
and send back.

## Nudges

The nudge list is separate from marking lurkers: anyone who isn't a marked
lurker can be on it, trusted or not, and it never gets them warned or
kicked. Each nudge has a length and a number of reminders to chat:

```text
nudge user=alice for=3w reminders=5
nudge list
nudge remove user=alice
```

- `for` is how long the nudge lasts, from 1 hour to 365 days, such as `36h`,
  `10d` or `3w`.
- `reminders` is how many reminders lurker sends in that time, from 1 to 20,
  at least an hour apart.
- Someone already being nudged has to be removed before being nudged again.
- Marking someone `trusted` or `untrusted` takes them off the nudge list, and
  marked lurkers can't be nudged.
- A nudge follows its nick when they change nick.

`nudge list` shows the nudges going, with how many reminders have been sent,
and the 20 that ended most recently, with how each went: whether they spoke
and how long after the first reminder, or why the nudge ended.

## Channels

Besides its two rooms, lurker joins the channels admins invite it to, with
`/invite lurker #channel` or `join #channel`, and ignores invitations from
anyone else. It remembers them and rejoins them after restarts and kicks.
`part #channel` makes it leave for good. As an IRC operator it also
[ops itself](how-it-works.md#its-channels) there.
