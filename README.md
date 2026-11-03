# lurkers

Time to clean up those lurkers.

`lurker` is an IRC bot that keeps watch over two rooms where IRC admins move
idle users:

- **#purgatory**, for people who used to be active but haven't been around
  for a long time. lurker is sad, apologetic and nostalgic here.
- **#FBI_watchlist**, for people nobody knows or can vouch for, who may be
  collecting data. lurker is an angry cop in an interrogation room here.

The spiritual inspiration is [IdleRPG](https://www.idlerpg.net/), where people
earned levels by sitting in a channel and saying nothing. lurker doesn't move
anyone itself: admins decide who goes where, and lurker keeps them company.

## Status

Early development. lurker connects to the server over TLS, joins both rooms,
and stays there: it rejoins a room it was kicked from or left, takes its nick
back when it was taken, reconnects after a lost connection, and quits with
"brb, getting a software update" when it is stopped. Admins mark lurkers and
record what they know about them with commands in private messages. lurker
doesn't say anything in the rooms yet; the rooms' messages come next.

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

## Staying Connected

- **Nick:** when the nick is taken, for example by lurker's own old
  connection that the server hasn't dropped yet, lurker connects as
  `<nick>_` or `<nick>__`. When it sees whoever has the nick quit or change
  nick, it takes the nick back.
- **Rooms:** lurker joins both rooms when it connects. Every minute it joins
  any room it isn't in, so it returns within a minute of being kicked, and
  keeps trying when a join fails, for example because it is banned. The
  server's reason for a failed join is logged.
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

Send these to lurker in a private message, such as `/msg lurker list`.

| Command | Does |
| --- | --- |
| `trusted <nick>` | Marks a lurker for #purgatory |
| `untrusted <nick>` | Marks a lurker for #FBI_watchlist |
| `channel <nick> <#channel> <date>` | Adds a channel the lurker idled in, or changes its date: when they last spoke there for a trusted lurker, or when they were first seen there for an untrusted one |
| `unchannel <nick> <#channel>` | Removes a channel |
| `peak <nick> <year> <daily\|weekly\|monthly>` | Records how often a trusted lurker chatted in the year they were most active |
| `info <nick>` | Shows everything recorded about a lurker |
| `list` | Lists the lurkers for each room, marking those in it now with `(here)` |
| `forget <nick>` | Deletes a lurker |
| `reload` | Reads the settings file again |
| `help` | Lists the commands |

Dates are `YYYY-MM-DD`, or a number of days ago such as `348d`, which lurker
turns into a date so the count keeps growing. Days are counted in UTC. The
numbers for `channel` and `peak` can come from `wot analyze --nick`.

Marking someone already marked for the other room moves them, and their time
in the room starts again.

## Lurkers

lurker only pays attention to the lurkers admins mark. Anyone else in the
rooms, such as people curious about what happens there, is left alone and
never recorded. lurker doesn't move anyone into or out of the rooms either;
admins do that.

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
| `peak_year`, `peak_frequency` | The year they were most active, and `daily`, `weekly` or `monthly` |
| `here_since` | When lurker first saw them in their room, or empty until then |
| `marked_by`, `marked_at` | The admin who last marked them, and when |

`lurker_channels` has the channels each lurker idled in:

| Column | Contents |
| --- | --- |
| `lurker_id` | The lurker |
| `channel` | The channel, as the admin gave it |
| `channel_key` | The channel lowercased with IRC's case mapping |
| `since` | `YYYY-MM-DD`: when they last spoke there (trusted) or were first seen there (untrusted) |

## Tests

```bash
cargo test
```

## License

MIT; see [LICENSE](LICENSE).
