# Commands

Admins send commands to lurker in a private message, such as
`/msg lurker list`, or in any channel lurker is in after its nick, such as
`lurker: list`, `lurker, list` or `lurker list`. lurker answers in the same
place, so in a channel everyone there sees the answer. Only the
[admins in the settings](configuration.md#admins) can use them, and lurker
logs every one.

| Command | Does |
| --- | --- |
| `trusted <nick> <fields>` | Marks or updates a lurker for #purgatory |
| `untrusted <nick> <fields>` | Marks or updates a lurker for #FBI_watchlist |
| `info <nick>` | Shows everything recorded about a lurker, ending with it all as one command |
| `list` | Lists the lurkers for each room, marking those in it now with `(here)` |
| `forget <nick>` | Deletes a lurker |
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
| `grace=<time>` | How long after a [warning](how-it-works.md#warnings-and-kicks) lurker kicks them from every channel and sajoins them into their room: such as `90m`, `12h`, `3d` or `2w`, or `never`. Giving it starts their warning over. |
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

## Channels

Besides its two rooms, lurker joins the channels admins invite it to, with
`/invite lurker #channel` or `join #channel`, and ignores invitations from
anyone else. It remembers them and rejoins them after restarts and kicks.
`part #channel` makes it leave for good. As an IRC operator it also
[ops itself](how-it-works.md#its-channels) there.
