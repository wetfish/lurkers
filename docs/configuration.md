# Configuration

lurker reads its settings from a TOML file, `lurker.toml` in the current
folder unless you give another path, as in `cargo run --release --
/path/to/lurker.toml`. Copy `lurker.example.toml`, which explains each
setting, and set the server and your admins. `.gitignore` excludes
`lurker.toml` and the default database.

| Setting | Default | Meaning |
| --- | --- | --- |
| `irc.server` | (required) | The IRC server's host name |
| `irc.port` | `6697` | The server's TLS port. lurker always connects over TLS. |
| `irc.nick` | `lurker` | lurker's nick |
| `irc.username` | the nick | The username (ident) |
| `irc.realname` | `lurker` | The real name shown in /whois |
| `rooms.purgatory` | `#purgatory` | The room for trusted lurkers |
| `rooms.watchlist` | `#FBI_watchlist` | The room for untrusted lurkers |
| `database.path` | `lurker.sqlite` | The [database](database.md), created when missing, relative to the folder lurker runs in |
| `http.listen` | none | Where to serve the [status page](#status-page), such as `0.0.0.0:8080` |
| `oper.name`, `oper.password` | none | lurker's [IRC operator](#irc-operator) login |
| `admins` | none | The [admins](#admins) |

lurker refuses to start with a mistake in the file, including a setting it
doesn't know, and says what is wrong. While it runs, it reads the file again
when it changes, within 30 seconds, or when an admin sends `reload`. It takes
the new admins straight away; everything else needs a restart. A changed
file with a mistake is logged and ignored.

lurker logs to standard error.

## Admins

Each admin has a nick and the hosts they connect from:

```toml
[[admins]]
nick = "rachel"
hosts = ["*.wetfish.net", "Fish-*.chat"]
```

A message only counts as an admin's when it comes from their nick and one of
their hosts. `*` matches any characters and `?` any one character, ignoring
case. A host of only wildcards, such as `*`, is refused, since it would let
anyone using the nick in. lurker ignores private messages from anyone else,
and logs that it did.

## Status page

With `http.listen` set, lurker serves a page that only says whether it is
connected to IRC, and shows nothing about anyone. `/up` answers 200 while it
is connected and 503 while it isn't. `lurker --health 127.0.0.1:8080` asks
it, and exits with 0 when lurker is connected; the container's health check
runs it.

## IRC operator

Warnings, kicks and sajoins need lurker to be an IRC operator. It logs in
with OPER as soon as it connects:

```toml
[oper]
name = "lurker"
password = "the plain password"
```

The IRC server runs InspIRCd 3.9 with `m_sajoin`, `m_sakick` and `m_samode`
loaded. lurker's oper block in `opers.conf` needs these commands and
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
| `SAJOIN` and `users/sajoin-others` | Sajoining lurkers into their room |
| `SAMODE` | Giving itself +o in its channels |
| `users/channel-spy` | Seeing a lurker's secret and private channels, so it kicks them from those too |
| `channels/auspex` | Seeing everyone in `+u` (auditorium) channels |

Opers with snomask `a` see a notice for every SAKICK and SAJOIN.
