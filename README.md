# lurkers

Time to clean up those lurkers.

`lurker` is an IRC bot for the Wetfish network. Admins mark people who sit
in channels without saying anything, and lurker moves them into one of two
rooms and keeps them company there until they speak:

- **#purgatory**, for people the community actually knows, who used to be
  active but haven't been around for a long time. Admins mark them
  `trusted`. lurker is sad, apologetic and nostalgic here.
- **#FBI_watchlist**, for people nobody knows or can vouch for, who may be
  collecting data. Admins mark them `untrusted`. lurker is an angry cop in an
  interrogation room here.

lurker never decides who is a lurker; admins do. The spiritual inspiration
is [IdleRPG](https://www.idlerpg.net/), where people earned levels by
sitting in a channel and saying nothing. lurker is in early development and
runs at `lurkers.wetfish.net`.

## How it works

1. An admin marks someone, such as `untrusted bob grace=3d silent=180d`.
2. The next time bob is online, lurker warns him.
3. Once his grace period of 3 days is over, lurker kicks him from every
   channel and sajoins him into #FBI_watchlist, again whenever he settles
   somewhere else.
4. In the room, lurker welcomes him and pings him about once a week.
5. When he says anything, lurker makes a fuss, tells the admins, and stops
   kicking him.

[How lurker works](docs/how-it-works.md) has the details.

## Marking lurkers

Admins send commands in a private message, or in a channel lurker is in,
after its nick:

```text
/msg lurker untrusted bob grace=3d silent=180d
/msg lurker trusted alice grace=7d silent=348d activity=daily year=2024
/msg lurker info alice
/msg lurker list
```

- `grace` is how long after the warning lurker starts kicking them: such as
  `90m`, `12h`, `3d` or `2w`, `0` to kick them without a warning, or `never`.
  Someone new needs one.
- `silent` is when they last spoke anywhere: `YYYY-MM-DD`, or days ago such
  as `180d`.
- `activity` and `year`, for trusted lurkers only and always together, say
  how often they chatted in the year they were most active.

[Commands](docs/commands.md) lists every command.

## Running lurker

You need Rust 1.89 or newer and a C compiler. Copy the example settings, set
`irc.server` and your admins, and run it:

```bash
cp lurker.example.toml lurker.toml
cargo run --release
```

To try it without disturbing the real rooms, give it another nick and test
rooms in `lurker.toml`. lurker warns and kicks nobody until it has an
[IRC operator login](docs/configuration.md#irc-operator).

Run the tests with `cargo test`.

## Docs

| Doc | Covers |
| --- | --- |
| [Commands](docs/commands.md) | Every admin command, and marking lurkers in full |
| [How lurker works](docs/how-it-works.md) | Warnings and kicks, what lurker says in the rooms, its channels |
| [Configuration](docs/configuration.md) | Settings, admins, the status page and the IRC operator login |
| [Messages](docs/messages.md) | Changing what lurker says: templates, placeholders and units |
| [Database](docs/database.md) | What lurker records about lurkers |
| [Deploying](docs/deploying.md) | Running lurker on a Wetfish server, updates and backups |

## License

MIT; see [LICENSE](LICENSE).
