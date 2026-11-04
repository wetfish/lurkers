# How lurker works

lurker only pays attention to the lurkers admins mark with
[`trusted` or `untrusted`](commands.md#marking-lurkers). Anyone else, such
as people curious about what happens in the rooms, is left alone and never
recorded.

A lurker is known by their nick, ignoring case. When lurker sees a lurker
change nick, it follows them to the new one. Someone who comes back under
another nick, such as `rachel_` after a dropped connection, isn't recognized
until they change back or an admin marks the new nick.

## Warnings and kicks

As an IRC operator, lurker deals with every marked lurker who has a grace
period, wherever they are:

1. **Warning:** as soon as lurker finds them online, which it checks every
   minute, it warns them in the voice of their room that it will take them
   there after their grace period. It warns them in a private message, so
   nobody else sees it. With `grace=0`, it skips the warning and kicks them
   straight away.
2. **Kicks:** once the grace period after the warning is over, lurker kicks
   them from every channel they are in apart from their room, including
   secret ones. It kicks as a channel op, so the Matrix bridge also removes
   Matrix users from the matching room, which it doesn't for a SAKICK. In a
   channel it isn't in, it sajoins itself, ops itself, kicks and leaves
   again. It does this again every 5 minutes while they are online, and
   straight away when they join one of lurker's channels, so they can't
   settle anywhere else.
3. **Invite:** lurker then invites them to their room, which the bridge
   passes on to Matrix users. If they haven't joined 30 to 60 seconds later,
   it sajoins them there; the log shows which happened. A lurker already in
   their room who was never invited, because lurker sajoined them before it
   sent invites, is kicked from the room once and invited back a minute
   later. The wait matters: the bridge withdraws an invite that reaches it
   before the kick does.
4. **Release:** a lurker who says something after their warning, in any
   channel lurker is in, isn't kicked any more. Marking them again with
   `grace=` starts over with a new warning.

Lurkers with `grace=never` are never warned or kicked. `info <nick>` says
where a lurker is in this. lurker logs every warning, kick and sajoin.

Without an [IRC operator login](configuration.md#irc-operator), lurker warns
and kicks nobody. When the server refuses the login, lurker tells every
admin who is online.

## In the rooms

lurker talks to lurkers in their own room only:

- **Welcome:** the first time a lurker joins their room after being marked,
  lurker welcomes them, to explain where they are, never with the same
  welcome as the lurker before them. A lurker who was already there isn't
  welcomed. Their time in the room starts when lurker first sees them in it.
- **Pings:** about once a week, each lurker gets a ping. The first comes 3 to
  7 days after they join their room, or within a week for someone who was
  already there, and the rest 7 to 9 days apart. A lurker who isn't in the
  room when one comes due gets it once they are back. Pings in the same room
  are at least 20 minutes apart, so a full room is never flooded.
- **Fanfare:** when a lurker speaks in their room, including with `/me`,
  lurker makes a fuss and sends every admin a notice. After that it stays
  quiet about them for a day.

The notice looks like this. Admins who aren't online miss it:

```text
#purgatory: alice spoke after 41 days in the room (silent 348 days): "hello? is anyone here"
#purgatory: carol spoke after 3 days in the room (no notes recorded): "hey"
```

What lurker says is in `messages.toml`; see [Messages](messages.md).

## Nudges

lurker reminds the people on the [nudge list](commands.md#nudges) to chat,
as an IRC operator. They don't have to be lurkers, and nudges never warn or
kick anyone.

1. **Reminders:** spread evenly over the nudge, such as one about every 4
   days for `for=3w reminders=5`. The first goes out as soon as lurker
   finds them online. Each one is a mention in every channel they are in,
   secret ones included, sajoining itself into those it isn't in.
2. **Listening:** lurker stays in those channels while the nudge is going,
   and rejoins them after a restart, so it hears them answer.
3. **They speak:** saying anything in a channel lurker is in, including
   with `/me`, ends the nudge, and every admin gets a notice:

   ```text
   Nudge: alice spoke in #anime 2 days after the first reminder, with 2 of 5 reminders sent, so the reminders stopped: "oh hi"
   ```

4. **It runs out:** when its time is up without a word, every admin gets a
   notice to review it:

   ```text
   Nudge: bob's nudge is over, with no reply after 5 reminders. Over to you to review; nudge list has the rest.
   ```

When a nudge ends for any reason, including `nudge remove` or marking them
as a lurker, lurker leaves the channels it was in only for it. A reminder
waits while they are offline, and while lurker sent another nudge reminder
in one of their channels in the last 20 minutes; reminders that come late
are still a whole gap apart. Admins who aren't online miss the notices, and
`nudge list` keeps how every nudge went.

## Its channels

lurker joins its two rooms and the [channels admins invite it
to](commands.md#channels), and rejoins any it isn't in every minute, such as
after a kick or a ban.

As an IRC operator, lurker gives itself +o with SAMODE in each of these
channels and in its rooms, where it needs +o to invite lurkers. If someone
takes +o away, it waits 10 minutes before taking it back, so it never fights
anyone over it. To kick or invite a lurker where it isn't opped, it ops
itself first.

## Staying connected

When its nick is taken, lurker connects as `lurker_` and takes its nick back
once it is free. When the connection drops, it reconnects, waiting longer
after each failed attempt, up to 5 minutes. When it is stopped, it quits
with "brb, getting a software update".
