![DiscoClip interface](https://user-images.githubusercontent.com/60236014/215372009-d6ca97db-f187-4c39-a8d9-d7ac31e5d52a.png)

# DiscoClip

DiscoClip downloads media links from Discord channels and the web app. It converts
clips to fit the destination, posts the results and can archive the files.

## Run

```sh
discoclip --config discoclip.toml
```

You can also set `DISCOCLIP_CONFIG=discoclip.toml`. See
[the example config](discoclip.example.toml) for available settings.

On first startup, open `/setup` and create the admin account.

## Build

```sh
cargo build --release
```

The build requires Node.js and npm. Cargo builds the SvelteKit web UI in
`crates/app/ui` and embeds it in the binary.

For web UI development, run the backend, then:

```sh
cd crates/app/ui
npm run dev
```

Vite proxies `/api` to `127.0.0.1:8080`. Set `DISCOCLIP_API` to use another address.

Web UI checks:

```sh
npm run check
npm run build
```

Follow [the UI and writing standards](crates/app/ui/DESIGN.md) when changing the interface.

## Discord

1. Open **Applications** and add a bot token from the Discord Developer Portal.
2. Use **Add to a server** to invite the bot.
3. Switch the server on to watch every channel in it, or open it and switch on single
   channels. A server watched whole can still have channels switched off.
4. **Options** on the server or a channel set where the media goes and who may post links.

Each application runs its own bot. Its page controls the bot, its settings and the
servers it is in. `/clip` and `/status` are registered globally when the application
is added; the API can move them to chosen servers or turn them off. A stopped bot
stays stopped until you start it.

The bot supplies channel and role names. Member searches use the Discord API with
a timeout. When a bot stops, its directory keeps the last known data.

Admins and operators can edit watch rules. A user with a linked Discord account
can also edit rules for servers they manage on Discord.

## Profiles

Profiles control platform access and media limits. Assignments apply in this order:

1. Global default
2. Discord server
3. Channel
4. Member

Each assignment overrides only the values it specifies. Blank limits and inherited
platform settings use the preceding assignment. Server limits cap every profile.

A profile can enable all platforms, disable all, inherit access, or select categories.
Individual platform exceptions override that choice. The built-in Default profile
enables all platforms and cannot be deleted.

Admins edit profiles and select the global default. Server managers and operators
assign profiles to a server, its channels and its members from the server page.
`GET /api/profiles/effective` reports what applies to a channel or member.

Jobs retain the limits and platform restrictions used when submitted. Retries use
current profiles. Playlist entries inherit the parent job settings. Older watch
rules with their own limits are migrated to channel profiles.

## Content views

Create a view under **Content views** to share completed media at `/f/<slug>`.
Pick a profile, the Discord servers or single channels it shows, and whether
downloads are allowed. With no server or channel picked it shows all media.

A view lets people in by any of these:

- Public access without login
- A shared PIN, password or access token
- View accounts, which exist only on that view
- Configured login providers

Discord login can require membership in all selected servers or restrict access
to listed users. Viewer sessions last 30 days. Admins manage view accounts
and end sessions on the view's page.

Enable **Discord links** to post a media page when an upload exceeds the size
limit or falls below the configured quality thresholds. These links require
`web.public_url`. The media page includes preview metadata for Discord.

Signed media links work without login until they expire, after 30 days by default.
Anyone with the link can access that file until expiry. When several views match,
a channel match takes priority over a server match, followed by unrestricted views.
Slug order breaks ties.

## Platforms and downloads

The Platforms page lists supported sites, media types, formats, login requirements
and test results. Checks resolve sample links without downloading media. They run
daily by default or on request.

| Media | Handling |
| --- | --- |
| Video | Choose a rendition and convert it to fit the destination |
| Audio | Download and process audio tracks |
| Image | Download the image |
| File | Download the original file |
| Playlist | Create one job per entry |
| Live stream | Record from submission until the stream ends or the capture limit is reached |

Supported transports include HTTP files, HLS, DASH, Smooth Streaming, RTMP, RTSP,
RTP and WHEP. HTTP downloads use parallel byte ranges when supported and resume
interrupted transfers. Changed files restart to avoid combining different versions.

Segmented streams support live recording, subtitles and discontinuities.
Supported transport encryption is decrypted during download. DRM-protected media
is rejected. Platform requests use the configured proxies, retries and rate limits.

Admins can import browser cookies from a Netscape `cookies.txt` file or a Cookie
header. Cookies and provider tokens are encrypted under `secret.key` and are not
shown again. The Platforms page can check or clear each saved session.

## Transcoding

Every container and codec ffmpeg decodes is taken as a source. The picture is
made to fit the destination:

| Source | Handling |
| --- | --- |
| HDR10, HLG and Dolby Vision profiles 7 and 8 | Tone-mapped to SDR |
| Dolby Vision profile 5 | Rendered with an ffmpeg build that has libplacebo, and refused by name without one |
| Interlaced pictures, flagged, or found by looking when the file does not say | Deinterlaced |
| Variable frame rates | Converted to a constant rate |
| 360° pictures: equirectangular, cubemap and YouTube's equi-angular cubemap | Rendered as a flat 100° view |
| Stereoscopic pictures | One eye is kept |
| Anamorphic pixels | Squared |
| Transparency | Laid over black |
| Subtitles, when a request asks to burn them in | Drawn into the picture from the fetched track, or from a stream inside the file |
| Sound alone, when the destination asks for video | Played over the cover art, the platform's thumbnail, or a waveform |

Every job's log says what was done and which encoder made the output.

Each destination has a target: the container, the video and audio codecs, a height
and frame-rate cap, whether sound alone becomes a video, and which audio, image and
other files are taken as they are. `local.target` covers web submissions and
`discord.target` covers Discord. A server can have its own upload limit and target
under `discord.guilds` by server id. Discord upload limits follow the server's boost
level under `discord.limits`; Nitro raises limits for people, not for bots.

Video is encoded with the embedded ffmpeg build in software. Set `engine.ffmpeg` to an
installed build and `engine.transcode.encoder` to `auto` or a family to use NVENC,
VA-API, Quick Sync, AMF, VideoToolbox or V4L2 encoders. Each encoder is tried at
startup and when the settings change. The Health page reports the encoder in use. A
hardware encode that fails during a job is redone in software and the job's log says so.

## Accounts

| Role | Access |
| --- | --- |
| Admin | Accounts, applications, settings, profiles, bots and jobs |
| Operator | Jobs, bots and watch rules |
| Viewer | Read access, plus rules for Discord servers they manage |

Use **Account** to change a password, manage sessions, link login providers and
create API tokens. Tokens use `Authorization: Bearer dc_<secret>` and are limited
to their selected scopes and the account role. See [the API reference](API.md).

GitHub, Google and OpenID Connect are configured in Settings. Discord login uses
an application with a client secret and login enabled.

Locked out of an account? The server prints a recovery key on its console, set
apart from the log, each time it starts. Open `/recover` and enter the key, the
username and a new password. That ends the account's sessions and the lockout
and logs you in. The key changes each time it is used.

## Settings and hosting

Settings are stored in the database under `data_dir`, which defaults to `data`.
TOML, YAML and JSON config files can seed values at startup. Environment variables
use `DISCOCLIP_<SECTION>__<KEY>` and override the config file. Values saved in the
web app take priority over provisioning values.

Settings apply when saved. Changes the server cannot apply are rejected before
storage. Use Settings to search, edit, import or export values. Exports include
secrets in plain text.

`web.bind` sets the listen address. Configure `[web.tls]` with PEM certificate and
key files for HTTPS. The server reloads changed certificates automatically.
Alternatively, serve HTTP behind a reverse proxy.

List proxy addresses or CIDR networks in `web.trusted_proxies` to accept
`Forwarded` and `X-Forwarded-*` headers. HTTPS connections use secure session cookies.
Proxies must pass `text/event-stream` responses without buffering. The web app
shares one event stream across browser tabs.

## Retention and backups

Retention runs on `engine.retention`: done jobs older than `jobs_days` and failed or
cancelled jobs older than `failed_jobs_days` are removed with their cached files, and
the cache is trimmed under `cache_max_bytes`, oldest jobs first. A sweep runs shortly
after startup and then every `sweep_interval_secs`. **Backups** shows the last sweep
and runs one on request.

The database is backed up on `backup`: one consistent copy every `interval_secs`, and one
at startup when the newest is older than that, into `backup.dir`, keeping the newest
`keep`. `secret.key` is copied beside the backups, since the secrets in a backup cannot be
read without it. **Backups** lists them, makes one on request and hands each out as a
file. To put one back:

```sh
discoclip restore data/backups/discoclip-20260924T171500Z.db
```

The restore checks the backup, refuses to run while the server holds the database, and
puts the key beside the database when it is missing there. Start the server afterwards.

## Deploying

The [Dockerfile](Dockerfile) builds the server with the web app embedded and runs it as an
unprivileged user with Chromium and fonts beside it. The [compose file](compose.yaml)
runs it with volumes for the data and cache directories:

```sh
docker compose up -d
```

For a host install, build the release binary and use the
[systemd unit](packaging/discoclip.service). Its comments carry the install steps. Both
set the data directory to `/var/lib/discoclip` and the cache to `/var/cache/discoclip`,
and both stop the server with a signal and a timeout that lets the graceful shutdown
finish.

A stop signal starts a graceful shutdown. The web listener stops taking connections
and closes its live feeds, jobs under way get `engine.shutdown.grace_secs` to finish,
and a job publishing its output finishes regardless. Jobs still running after the
grace period are queued again and picked up at the next start. Then the bots close
their gateway connections, the database is checkpointed, and the process exits. During
the shutdown `GET /healthz` answers `503` with `stopping`. A second signal exits at once.

`GET /healthz` takes no credentials and answers `200` while the server works, `503` when
a part has failed or the server is stopping. The container's health check and the
readiness probe of an orchestrator use it.

`GET /metrics` serves the Prometheus text exposition to anything that does not ask for
HTML: job counts by status, resolver outcomes, outgoing requests, bots, cache, database,
encoder, retention, backups and every health check. It takes the same credentials as the
API, so Prometheus scrapes it with an API token, as in the
[example scrape config](packaging/prometheus.yml). Browsers at the same address get the
Metrics page.

## Monitoring

- **Health** checks the database, engine, storage, ffmpeg, the video encoder, subtitle fonts, bots, platform tests, retention and backups.
- **Metrics** shows resource use, job counts, requests and storage, refreshed every five seconds.
- **Server log** shows up to 5,000 retained lines with filters and live updates.
- **Audit log** records who changed settings, applications, rules, profiles and access.

Audit entries are stored with the associated changes. Secrets are redacted.
Admins can filter and page through entries in the app or at `GET /api/audit`.
