# Deploying lurker

lurker runs on a [Wetfish server](https://github.com/wetfish/server) as a
project named `lurker`, on `lurkers.wetfish.net`, deploying the `main`
branch whenever it is pushed.

## What runs

- **One container**, `lurker-lurker-1`, built from the `Dockerfile`. It
  runs as root inside the container, since deploys write the settings file
  readable only by root, and it can reach nothing on the server but its own
  volume and its settings.
- **The status page** on port 8080, which the server's Caddy serves on
  `https://lurkers.wetfish.net/`. It only says whether lurker is connected
  to IRC, and shows nothing about anyone in its rooms or channels. `/up`
  answers 200 while lurker is connected and 503 while it isn't; the
  container's health check asks it with `lurker --health`, so a deploy only
  succeeds once lurker is on IRC. The same domain takes the GitHub webhook
  that deploys lurker.
- **The `data` volume**, Docker's `lurker_data`, at `/data` in the
  container, with the database.
- **The settings**, `config/lurker/settings/lurker.toml` on the server,
  which every deploy copies into the project's clone as
  `settings/lurker.toml`. The container reads it from `/settings`, mounted
  read-only.

When a deploy replaces the container, lurker quits IRC with "brb, getting a
software update" and the new container connects again, which takes a few
seconds.

## Adding lurker to a server

The commands run as root on the server, in `/srv/wetfish-server`.

1. Point `lurkers.wetfish.net` at the server in DNS.

2. Stop any other copy of lurker, such as one on your computer, so the two
   don't fight over the nick. To bring its data along, see
   [Moving from your computer](#moving-from-your-computer) first.

3. Write the settings, before adding the project, so its first deploy can
   connect:

   ```bash
   mkdir -p config/lurker/settings
   nano config/lurker/settings/lurker.toml
   chmod 600 config/lurker/settings/lurker.toml
   ```

   Lay it out like `lurker.example.toml`, with the real rooms, and with
   these two settings, which the container needs:

   ```toml
   [database]
   path = "/data/lurker.sqlite"

   [http]
   listen = "0.0.0.0:8080"
   ```

4. Add the project:

   ```bash
   ./add-project.sh --name lurker --repo https://github.com/wetfish/lurkers.git \
       --domain lurkers.wetfish.net --branch main --upstream lurker:8080
   ```

   It builds lurker, starts it, waits for it to connect to IRC, and prints
   the webhook to add in GitHub, under the repository's Settings → Webhooks.

5. Check that `https://lurkers.wetfish.net/` says lurker is connected to
   IRC, and follow what lurker does:

   ```bash
   docker logs -f lurker-lurker-1
   ```

If the first deploy fails, for example because of a mistake in
`lurker.toml`, the log says why. The project stays added; fix the problem
and deploy again with `./deploy.sh lurker`.

## Moving from your computer

To keep the lurkers and channels a copy of lurker on your computer knows:

1. While your copy still runs, tidy up what was only for testing, so the
   server's lurker doesn't join test channels or carry test lurkers into the
   real rooms. `/msg lurker channels` lists its channels, and `list` its
   lurkers:

   ```text
   /msg lurker part #lurker-test
   /msg lurker forget <test nick>
   ```

   Rooms are stored as purgatory or watchlist rather than by channel name,
   so lurkers marked in test rooms move into the real rooms. Their time in
   the room still counts from when they joined the test room.

2. Stop your copy with Ctrl+C, so it writes nothing more, and copy its
   database to the server:

   ```bash
   scp lurker.sqlite root@<server>:/root/lurker.sqlite
   ```

3. Add lurker to the server as above. It starts with an empty database.

4. Replace that database with yours, while lurker is stopped:

   ```bash
   docker stop lurker-lurker-1
   cp /root/lurker.sqlite "$(docker volume inspect --format '{{.Mountpoint}}' lurker_data)/lurker.sqlite"
   docker start lurker-lurker-1
   ```

5. Check that `/msg lurker list` shows your lurkers and `/msg lurker
   channels` your channels.

A database from an older version of lurker is brought up to date when
lurker opens it. One from a newer version is refused, so copy it to a server
running the same version or newer.

## Changing the settings

Edit `config/lurker/settings/lurker.toml` and deploy:

```bash
./deploy.sh lurker
```

The deploy copies the file into the clone. When nothing else changed, the
container stays as it is, and lurker reads the file again within 30
seconds without leaving IRC, taking the new admins; an admin can send
`reload` to have it read the file straight away. Changes to `[irc]`,
`[rooms]`, `[database]` and `[http]` need a restart, which reconnects:

```bash
docker restart lurker-lurker-1
```

## Updates

Pushing to `main` deploys. To deploy by hand, or follow a deploy:

```bash
./deploy.sh lurker
journalctl -u project-deploy@lurker -f
```

To go back to an earlier commit, deploy it, then push a fix or a revert so
the next push doesn't bring the change back:

```bash
./deploy.sh lurker 3f9c2a1b7d4e
```

A newer lurker may have upgraded the database, which an older one refuses.
Back it up before deploying a change that does, and put the backup back
when rolling back.

## Backups

The database is one file. Copy it while lurker is stopped, so the copy is
whole:

```bash
docker stop lurker-lurker-1
cp "$(docker volume inspect --format '{{.Mountpoint}}' lurker_data)/lurker.sqlite" /root/lurker-$(date +%F).sqlite
docker start lurker-lurker-1
```

## Trying it in Docker on your computer

`docker-compose.override.yml`, which deploys leave out, publishes the status
page on `http://localhost:8080/`. Put your settings in `settings/lurker.toml`,
which `.gitignore` excludes, with `http.listen = "0.0.0.0:8080"`, then:

```bash
docker compose up --build
```

The database is then in the `data` volume of the folder's Compose project,
not in your `lurker.sqlite`.
