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
"brb, getting a software update" when it is stopped. It keeps track of who is
in the rooms in a SQLite database, but doesn't say anything yet. Op commands
and the rooms' messages come next.

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

Settings lurker doesn't know are an error, so typos don't go unnoticed.
lurker checks the file when it starts and exits with a message if anything is
wrong.

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

## Who Is in the Rooms

When lurker joins a room it asks the server who is there, then follows
joins, parts, kicks, quits, nick changes, host changes and account changes.
It leaves itself out, including an old connection of its own that the
server hasn't dropped yet.

Each person in a room is linked to a user in the database:

1. Someone already in the other room under the same nick is the same
   connection, so the same user.
2. Otherwise, the user last seen with exactly the same nick and
   `ident@host`. Nicks are compared without regard to case, and hosts too.
3. Otherwise, a new user.

A user who changes nick or host while lurker can see them keeps their
record, and the new nick or host is added to it, so they are recognized
under either later. Someone who comes back under a new nick or from a new
host without lurker seeing the change is a new user for now; recognizing
reconnects such as `rachel` coming back as `rachel_` comes in a later
version.

lurker asks the server for three IRCv3 capabilities, and logs which ones it
enabled. Without them lurker still works, but knows less:

| Capability | Without it |
| --- | --- |
| `chghost` | A host change looks like a quit and a join, so the user's new host isn't added to their record |
| `extended-join`, `account-notify` | lurker doesn't learn people's services accounts as they join, or when they log in or out |

When the server supports WHOX, lurker uses it to list who is in a room,
which also gives everyone's services account.

lurker logs who joins and leaves the rooms, and anyone it sees in a room for
the first time. After joining a room, it logs how many people are in it.

## Database

lurker keeps what it knows about the users in its rooms in a SQLite
database, so it survives restarts and reconnects. You can open it with any
SQLite client, such as `sqlite3 lurker.sqlite`.

The database records its schema version. A newer version of lurker brings
an older database up to date when it opens it. A database made by a newer
version of lurker than the one running is refused.

Times are whole seconds since 1970 UTC. In `sqlite3`,
`datetime(first_joined_at, 'unixepoch')` shows one as a date and time.

`users` has one row per person lurker has seen in a room:

| Column | Contents |
| --- | --- |
| `id` | The user's number |
| `nick` | The nick they were last seen with |
| `account` | Their services account, while the server says they are logged in |
| `created_at` | When lurker first saw them |

`user_keys` has every nick and `ident@host` combination each user has been
seen with, which is how lurker recognizes them when they come back:

| Column | Contents |
| --- | --- |
| `user_id` | The user |
| `nick` | The nick, lowercased with IRC's case mapping |
| `ident`, `host` | The ident as given, and the host lowercased |
| `first_seen_at`, `last_seen_at` | When lurker first and last saw this combination |

`memberships` has one row for each room a user has been in:

| Column | Contents |
| --- | --- |
| `user_id` | The user |
| `room` | `purgatory` or `watchlist`, rather than the channel name, so renaming a room in the settings keeps its users |
| `first_joined_at` | When lurker first saw them in the room. Time in the room counts from here, even if they have left and come back since. |
| `last_seen_at` | When lurker last saw them in the room: when they joined or left, or when lurker left or lost its connection |

## Tests

```bash
cargo test
```

## License

MIT; see [LICENSE](LICENSE).
