# lurkers

Time to clean up those lurkers.

`lurker` is an IRC bot that keeps watch over two rooms where IRC admins move
idle users:

- **#purgatory**, for people the community actually knows, who used to be
  active but haven't been around for a long time. Admins mark them
  `trusted`. lurker is sad, apologetic and nostalgic here.
- **#FBI_watchlist**, for people nobody knows or can vouch for, who may be
  collecting data, even if they have said a few things before. Admins mark
  them `untrusted`. lurker is an angry cop in an interrogation room here.

The spiritual inspiration is [IdleRPG](https://www.idlerpg.net/), where people
earned levels by sitting in a channel and saying nothing. lurker doesn't move
anyone itself: admins decide who goes where, and lurker keeps them company.

## Status

Early development. lurker connects to the server over TLS, joins both rooms,
and stays there: it rejoins a room it was kicked from or left, takes its nick
back when it was taken, reconnects after a lost connection, and quits with
"brb, getting a software update" when it is stopped. Admins mark lurkers and
record what they know about them with commands, and lurker welcomes each
lurker to their room, makes a fuss when one speaks, and pings each one about
once a week. As an [IRC operator](#irc-operator), it warns marked lurkers,
then kicks them from every channel and sajoins them into their room until
they speak, and ops itself in the channels admins invite it to. It runs on a
Wetfish server at `lurkers.wetfish.net`; see [Deploying](docs/deploying.md).

## Requirements

- Rust 1.89 or newer (edition 2024)
- A C compiler. lurker's TLS library, rustls, uses aws-lc-rs for its
  cryptography, which is compiled from C, and SQLite is compiled into lurker
  through rusqlite's `bundled` feature, so no system SQLite library is
  needed. CMake is not needed either.

## Building

```bash
cargo build --release
```

The binary is written to `target/release/lurker`.

## Configuration

lurker reads its settings from a TOML file. Copy the example and set the
server:

```bash
cp lurker.example.toml lurker.toml
```

`.gitignore` excludes `lurker.toml` and the default database.

| Setting | Default | Meaning |
| --- | --- | --- |
| `irc.server` | (required) | The IRC server's host name |
| `irc.port` | `6697` | The server's TLS port. lurker always connects over TLS. |
| `irc.nick` | `lurker` | lurker's nick |
| `irc.username` | the nick | The username (ident) |
| `irc.realname` | `lurker` | The real name shown in /whois |
| `rooms.purgatory` | `#purgatory` | The room for people who used to be active |
| `rooms.watchlist` | `#FBI_watchlist` | The room for people nobody knows |
| `database.path` | `lurker.sqlite` | The [database](#database), created when missing. A relative path is relative to the folder lurker runs in. |
| `http.listen` | none | Where to serve the [status page](#status-page), such as `0.0.0.0:8080`. Without it, there is no page. |
| `oper.name`, `oper.password` | none | lurker's [IRC operator](#irc-operator) login. Without it, lurker warns and kicks nobody. |
| `admins` | none | The [admins](#admins), each in its own `[[admins]]` section with a `nick` and `hosts` |

Settings lurker doesn't know are an error, so typos don't go unnoticed.
lurker checks the file when it starts and exits with a message if anything is
wrong.

While it runs, lurker checks the file every 30 seconds and reads it again
when it changed, or when an admin sends `reload`. It takes the new list of
admins straight away. Changes to `[irc]`, `[rooms]` and `[database]` take
effect when lurker restarts, which it logs. If the changed file has a
mistake, lurker logs it and keeps its previous settings.

## Running

```bash
cargo run --release
cargo run --release -- /path/to/lurker.toml
```

Without an argument, lurker reads `lurker.toml` from the current folder. It
logs what it does to standard error. Ctrl+C or SIGTERM, which Docker sends
when it stops a container, makes lurker quit with "brb, getting a software
update".

To try lurker without disturbing the real rooms, give it another nick and
other rooms in `lurker.toml`, such as `#purgatory-test` and
`#watchlist-test`.

To run it in Docker, or on a Wetfish server, see
[Deploying](docs/deploying.md).

### Status page

With `http.listen` set, lurker serves a page that only says whether it is
connected to IRC, and shows nothing about anyone in its rooms or channels.
`/up` answers 200 while lurker is connected and 503 while it isn't, for
health checks. `lurker --health 127.0.0.1:8080` asks it, and exits with 0
when lurker is connected; the container's health check runs it.

## Staying Connected

- **Nick:** when the nick is taken, for example by lurker's own old
  connection that the server hasn't dropped yet, lurker connects as
  `<nick>_` or `<nick>__`. When it sees whoever has the nick quit or change
  nick, it takes the nick back.
- **Rooms:** lurker joins both rooms, and its [channels](#channels), when it
  connects. Every minute it joins any it isn't in, so it returns within a
  minute of being kicked, and keeps trying when a join fails, for example
  because it is banned. The server's reason for a failed join is logged.
- **Reconnecting:** when the connection is lost, lurker waits 5 seconds and
  connects again, doubling the wait after each attempt up to 5 minutes. A
  connection that lasted 5 minutes resets the wait to 5 seconds. Connecting,
  including the TLS handshake, gives up after 30 seconds.
- **Pings:** lurker pings the server every 3 minutes, and treats the
  connection as lost if no answer comes within 20 seconds.

IRC compares names without regard to case, so lurker treats `#FBI_Watchlist`
and `#FBI_watchlist` as the same room, and also treats `[]\~` as the same as
`{}|^`, as servers using the `rfc1459` case mapping do.

## Admins

Admins mark lurkers and give lurker commands in private messages. Each admin
in the settings has a nick and the hosts they connect from:

```toml
[[admins]]
nick = "rachel"
hosts = ["*.wetfish.net", "Fish-*.chat"]
```

A message only counts as an admin's when it comes from their nick and one of
their hosts. In hosts, `*` matches any characters and `?` any one character,
ignoring case. A host of only wildcards, such as `*`, is refused, since it
would let anyone using the nick in. lurker ignores private messages from
anyone else, and logs that it did.

lurker logs every command an admin sends.

## Commands

Send these to lurker in a private message, such as `/msg lurker list`, or
in any channel lurker is in, starting with its nick, such as
`lurker: list`, `lurker, list` or `lurker list`. lurker answers in the same
place, so in a channel everyone there sees the answer.

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

### Marking lurkers

`trusted` and `untrusted` take the nick, then any of these fields, in any
order:

| Field | Means |
| --- | --- |
| `grace=<time>` | How long after a [warning](#warnings-and-kicks) lurker kicks them from every channel and sajoins them into their room: minutes, hours, days or weeks, such as `90m`, `12h`, `3d` or `2w`, or `never`. Giving it starts their warning over. |
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

Marking someone new needs `grace`, so each lurker can be given their own.
Marking someone already marked changes only the fields given and keeps the
rest, so `untrusted bob grace=never` only changes bob's grace period and
`untrusted bob silent=30d` only changes when he went silent. lurker checks
the whole command before changing anything, so a mistake anywhere in it
changes nothing.

Marking someone already marked for the other room moves them, and their time
in the room, their pings and their warning start again. Their grace period
and silent date stay.

`info` ends with everything recorded about a lurker as one command, such as
`As one command: untrusted bob grace=3d silent=2026-05-06`,
to copy, change and send back.

## Channels

Besides its two rooms, lurker joins the channels admins ask it to, by
inviting it with `/invite lurker #channel` or with `join #channel`. It
ignores invitations from anyone else. It remembers these channels in its
database and joins them, with its rooms, whenever it connects, including
after a restart. Every minute it rejoins any it isn't in, such as after
being kicked. `part #channel` makes it leave for good.

As an IRC operator, lurker gives itself +o with SAMODE in each of these
channels where it isn't opped, but not in its rooms. If someone takes +o
away, it waits 10 minutes before taking it back, so it never fights anyone
over it. It learns its status from the list of names when it joins and from
mode changes after that, and asks the server for the `multi-prefix`
capability so the list of names shows all of it.

## Warnings and kicks

As an IRC operator, lurker deals with every marked lurker who has a grace
period, wherever they are:

1. **Warning:** as soon as lurker finds them online, it warns them, in the
   voice of their room, that it will take them to their room after their
   grace period. It warns them in one of its channels if they are in one,
   and otherwise in a private message. It checks every minute which lurkers
   are online, with ISON.
2. **Kicks:** once their grace period after the warning is over, lurker
   looks them up with WHOIS and kicks them, with SAKICK, from every channel
   they are in apart from their room, including secret ones, then sajoins
   them into their room if they aren't in it. It looks them up again every 5
   minutes while they are online, and straight away when they join one of
   lurker's channels, so they can't settle anywhere else.
3. **Release:** a lurker who says something after their warning, in any
   channel lurker is in, isn't kicked any more. Most will speak in their
   room, where lurker makes a fuss and tells the admins. Marking them again
   with `grace=` starts over with a new warning.

Lurkers with no grace period are never warned or kicked, and lurker never
acts on its own nick. `info <nick>` says where a lurker is in this. lurker
logs every warning, kick and sajoin, and the server notices about its
commands, such as one it wasn't allowed to use.

Without an IRC operator login, or when the server refuses it, lurker warns
and kicks nobody. When the server refuses it, lurker tells every admin who
is online.

## IRC operator

lurker logs in with OPER as soon as it connects, with `name` and `password`
from `[oper]` in its settings:

```toml
[oper]
name = "lurker"
password = "the plain password"
```

On the IRC server, which runs InspIRCd 3.9 with `m_sajoin`, `m_sakick` and
`m_samode` loaded, its oper block in `opers.conf` needs these commands and
privileges, and nothing more:

```text
<class name="LurkerBot"
     commands="SAJOIN SAKICK SAMODE"
     privs="users/sajoin-others users/channel-spy channels/auspex">

<type name="Lurker_Bot" classes="LurkerBot" vhost="lurkers.wetfish.net">

<oper name="lurker"
      hash="hmac-sha256"
      password="<the output of /MKPASSWD hmac-sha256 <password>>"
      host="lurker@<the address lurker connects from>"
      sslonly="yes"
      type="Lurker_Bot">
```

| Needed | For |
| --- | --- |
| `SAKICK` | Kicking lurkers from channels lurker isn't in |
| `SAJOIN` and `users/sajoin-others` | Sajoining lurkers into their room; InspIRCd 3.9 only lets an oper sajoin themselves without the privilege |
| `SAMODE` | Giving itself +o in its channels |
| `users/channel-spy` | Seeing a lurker's secret and private channels in WHOIS, so it kicks them from those too |
| `channels/auspex` | Seeing everyone in `+u` (auditorium) channels |

The server notices every SAKICK and SAJOIN to opers with snomask `a`.

## Lurkers

lurker only pays attention to the lurkers admins mark. Anyone else in the
rooms, such as people curious about what happens there, is left alone and
never recorded. lurker only puts lurkers in their room once they have been
warned and their grace period is over; see
[Warnings and kicks](#warnings-and-kicks).

A lurker is known by their nick, ignoring case. When lurker sees a lurker
change nick in one of its rooms, it follows them to the new nick. Someone who
comes back under another nick, for example `rachel_` after a dropped
connection, isn't recognized until they change back or an admin marks the
new nick.

A lurker's time in their room starts the first time lurker sees them there
after they were marked, whether they join or were already there. lurker
learns who is in a room from the server's list of names when it joins, then
follows joins, parts, kicks, quits and nick changes. It logs lurkers coming
and going, and after joining a room, how many people and lurkers are in it.

## Messages

lurker talks to lurkers in their own room only, and never to anyone else:

- **Welcome:** the first time a lurker joins their room after being marked,
  lurker welcomes them, to explain where they are. A lurker who was already
  in the room, when they were marked or when lurker joined, isn't welcomed.
- **Fanfare:** when a lurker speaks in their room, including with `/me`,
  lurker makes a fuss about it and sends every admin a notice with what they
  said. After that it stays quiet about them for a day, so a lurker who keeps
  talking isn't drowned in fanfare. Admins who aren't online miss the
  notice; it isn't kept for later.
- **Warnings and kicks:** see [Warnings and kicks](#warnings-and-kicks).
  Their messages are in the voice of the lurker's room, wherever lurker is.
- **Pings:** each lurker is pinged about once a week. The first ping comes 3
  to 7 days after they first join their room; a lurker who joined longer ago
  than that, such as one marked while sitting there for weeks, is pinged
  some time in the next week instead. Each ping after that comes 7 to 9 days
  after the last. A lurker who isn't in their room when their ping comes due
  is pinged once they are back. Pings in the same room are at least 20
  minutes apart, so a room with many lurkers is never flooded.

The admin notice looks like this:

```text
#purgatory: alice spoke after 41 days in the room (silent 348 days): "hello? is anyone here"
#FBI_watchlist: bob spoke after 12 days in the room (silent 180 days): "uh hi"
#purgatory: carol spoke after 3 days in the room (no notes recorded): "hey"
```

### Templates

The messages are in `messages.toml`, which is built into lurker, so changing
them takes a rebuild. Each room has a list of pings, welcomes, fanfares,
warnings and kick reasons, and lurker picks one at random. Each lurker hears every ping in their room's
list before any repeats.

Placeholders in braces are filled in for the lurker. A message is only used
when everything it needs is recorded, so a lurker without notes only gets
the messages that need none, and gets more as admins add when they went
silent and how active they were.

| Placeholder | Room | Renders as |
| --- | --- | --- |
| `{nick}` | both | The lurker's nick |
| `{room}` | both | The channel name of the lurker's room |
| `{grace}` | both | Their grace period, such as `3 days` or `90 minutes` |
| `{here}` | both | Time since they first joined their room |
| `{idle}` | #purgatory | Time since they went silent, from `silent=` |
| `{last_spoke}` | #purgatory | The date they went silent, such as `March 3, 2025` |
| `{peak_year}` | #purgatory | The year they were most active, from `year=` |
| `{peak_frequency}` | #purgatory | `every day`, `every week` or `every month`, from `activity=` |
| `{lurking}` | #FBI_watchlist | The same as `{idle}` |
| `{silent_since}` | #FBI_watchlist | The same as `{last_spoke}` |

In #FBI_watchlist, times, dates and `AND` are in capitals to match the
shouting. Nicks and channels keep their case, so highlights still work.

Times are in days, apart from `{grace}`, which is in the largest of days,
hours and minutes it is a whole number of, unless the template names a unit
after a colon, as in `{idle:seconds}`. `{idle:silly}` picks one of these at random each time,
among those that come to at least 2. Numbers are rounded down and get
thousands separators, such as `30,067,200 seconds`.

| Unit | Length |
| --- | --- |
| `seconds`, `minutes`, `hours`, `days` | As named |
| `heartbeats` | 70 a minute |
| `hummingbird_heartbeats` | 1,200 a minute |
| `bitcoin_blocks` | 10 minutes |
| `microcenturies` | 52.6 minutes |
| `iss_orbits` | 92.9 minutes, said as "orbits of the ISS" |
| `shifts` | 8 hours, said as "eight-hour shifts" |
| `mayfly_lifetimes` | 1 day |
| `sols` | 24 hours 39 minutes 35 seconds, said as "sols on Mars" |
| `iodine_half_lives` | 8.02 days, said as "half-lives of iodine-131" |
| `fruit_fly_generations` | 10 days, said as "generations of fruit flies" |
| `mosquito_generations` | 14 days, said as "generations of mosquitoes" |
| `fortnights` | 14 days |
| `menstrual_cycles` | 28 days |
| `full_moons` | 29.53 days |
| `mercury_years` | 88 days, said as "years on Mercury" |
| `dog_years` | 365.25 / 7 days |

`cargo test` checks that every template only uses placeholders and units its
room has, and fills each in for a lurker with everything recorded.

## Database

lurker keeps the lurkers in a SQLite database, so they survive restarts. You
can open it with any SQLite client, such as `sqlite3 lurker.sqlite`.

The database records its schema version. A newer version of lurker brings an
older database up to date when it opens it. A database made by a newer
version of lurker than the one running is refused.

Times are whole seconds since 1970 UTC. In `sqlite3`,
`datetime(marked_at, 'unixepoch')` shows one as a date and time.

`lurkers` has one row per lurker:

| Column | Contents |
| --- | --- |
| `id` | The lurker's number |
| `nick` | Their nick, as the admin gave it or as they changed it to |
| `nick_key` | The nick lowercased with IRC's case mapping, to look them up |
| `room` | `purgatory` for a trusted lurker or `watchlist` for an untrusted one, rather than the channel name, so renaming a room in the settings keeps its lurkers |
| `peak_year`, `peak_frequency` | The year they were most active, and `daily`, `weekly` or `monthly`, from `year=` and `activity=` |
| `here_since` | When lurker first saw them in their room, or empty until then |
| `marked_by`, `marked_at` | The admin who last marked them, and when |
| `next_ping_at` | When lurker next pings them, set once their time in the room has started |
| `fanfare_at` | When lurker last made a fuss about them speaking |
| `grace` | How many seconds after their warning they are kicked from every channel, or empty for never |
| `warned_at` | When lurker warned them, or empty until it does, and again after a new grace period or a move to the other room |
| `spoke_at` | When lurker last saw them say something, in any channel it is in. After `warned_at`, it stops the kicks. |
| `silent_since` | `YYYY-MM-DD` from `silent=`: when they went silent, which is when they last spoke anywhere, or were first seen if they never spoke |

Lurkers used to be recorded with a date for each channel they idled in, in a
`lurker_channels` table. Upgrading an older database keeps the most recent of
each lurker's channel dates as their `silent_since` and drops that table.

`used_pings` has the ping messages each lurker has heard since they last heard
every one they can, by `lurker_id` and `template`, the message's position in
its list in `messages.toml`, counting from 0. Marking a lurker for the other
room clears theirs.

`channels` has the channels admins asked lurker to join besides its rooms:
`channel_key` (the name lowercased with IRC's case mapping), `channel` (the
name as the admin gave it), and `added_by` and `added_at`.

## Tests

```bash
cargo test
```

## License

MIT; see [LICENSE](LICENSE).
