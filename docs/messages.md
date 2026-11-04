# Messages

Everything lurker says to lurkers is in `messages.toml`, which is built into
lurker, so a change takes a rebuild and deploy. Each room has a list of
pings, welcomes, fanfares, warnings and kick reasons, and lurker picks one
at random. Each lurker hears every ping in their room's list before any
repeats.

## Placeholders

Placeholders in braces are filled in for the lurker. A message is only used
when everything it needs is recorded, so a lurker without a silent date or
activity only gets the messages that need neither.

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

In #FBI_watchlist, times and dates are in capitals to match the shouting.
Nicks and channels keep their case, so highlights still work.

## Units

Times are in days, and `{grace}` in the largest of days, hours and minutes
it is a whole number of. A template can name another unit after a colon, as
in `{idle:seconds}`, or `{idle:silly}` for one picked at random each time
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
