# Database

lurker keeps the lurkers in one SQLite file, so they survive restarts. Open
it with any SQLite client, such as `sqlite3 lurker.sqlite`.

A newer version of lurker brings an older database up to date when it opens
it, and refuses one made by a newer version than itself. Back the database
up before deploying a change that upgrades it; see
[Deploying](deploying.md#updates).

Times are whole seconds since 1970 UTC. In `sqlite3`,
`datetime(marked_at, 'unixepoch')` shows one as a date and time.

## lurkers

One row per lurker:

| Column | Contents |
| --- | --- |
| `id` | The lurker's number |
| `nick` | Their nick, as the admin gave it or as they changed it to |
| `nick_key` | The nick lowercased with IRC's case mapping, to look them up |
| `room` | `purgatory` or `watchlist`, rather than the channel name, so renaming a room in the settings keeps its lurkers |
| `silent_since` | `YYYY-MM-DD` from `silent=`: when they last spoke anywhere, or were first seen if they never spoke |
| `peak_year`, `peak_frequency` | From `year=` and `activity=`: the year they were most active, and `daily`, `weekly` or `monthly` |
| `here_since` | When lurker first saw them in their room, or empty until then |
| `marked_by`, `marked_at` | The admin who last marked them, and when |
| `next_ping_at` | When lurker next pings them, set once their time in the room has started |
| `fanfare_at` | When lurker last made a fuss about them speaking |
| `grace` | How many seconds after their warning they are kicked, or empty for never |
| `warned_at` | When lurker warned them, or empty until it does, and again after a new grace period or a move to the other room |
| `spoke_at` | When lurker last saw them say something, in any channel it is in. After `warned_at`, it stops the kicks. |

Older databases had a date for each channel a lurker idled in, in a
`lurker_channels` table. Upgrading keeps the most recent of each lurker's
dates as `silent_since` and drops the table.

## used_pings

The ping messages each lurker has heard since they last heard every one
they can, by `lurker_id` and `template`: the message's position in its list
in `messages.toml`, counting from 0. Moving a lurker to the other room
clears theirs.

## channels

The channels admins asked lurker to join besides its rooms: `channel_key`
(the name lowercased with IRC's case mapping), `channel` (the name as the
admin gave it), `added_by` and `added_at`.
