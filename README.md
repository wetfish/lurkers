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
"brb, getting a software update" when it is stopped. Tracking the users in the
rooms, op commands and the rooms' messages come next.

## Requirements

- Rust 1.89 or newer (edition 2024)
- A C compiler. lurker's TLS library, rustls, uses aws-lc-rs for its
  cryptography, which is compiled from C. CMake is not needed.

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

`.gitignore` excludes `lurker.toml`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `irc.server` | (required) | The IRC server's host name |
| `irc.port` | `6697` | The server's TLS port. lurker always connects over TLS. |
| `irc.nick` | `lurker` | lurker's nick |
| `irc.username` | the nick | The username (ident) |
| `irc.realname` | `lurker` | The real name shown in /whois |
| `rooms.purgatory` | `#purgatory` | The room for people who used to be active |
| `rooms.watchlist` | `#FBI_watchlist` | The room for people nobody knows |

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

## Tests

```bash
cargo test
```

## License

MIT; see [LICENSE](LICENSE).
