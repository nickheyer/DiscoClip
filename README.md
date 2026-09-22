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

On first startup, the server prints a setup token. Open `/setup`, enter the token
and create an admin account.

## Build

```sh
cargo build --release
```

The build requires Node.js and npm. Cargo builds the SvelteKit frontend in
`crates/app/ui` and embeds it in the binary.

For frontend development, run the backend, then:

```sh
cd crates/app/ui
npm run dev
```

Vite proxies `/api` to `127.0.0.1:8080`. Set `DISCOCLIP_API` to use another address.

Frontend checks:

```sh
npm run check
npm run build
```

Follow [the UI and writing standards](crates/app/ui/DESIGN.md) when changing the interface.

## Discord

1. Open **Applications** and add a bot token from the Discord Developer Portal.
2. Use **Add to server** to install the bot.
3. Open a server and choose a channel to watch.
4. Set the destination and allowed members in its watch rule.

Each application runs its own bot. Its page controls the bot, credentials and
registration of `/clip` and `/status`. Commands can be registered globally or in
selected servers. A stopped bot stays stopped until you start it.

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
assign profiles from the server page. The profile preview shows the effective
settings for a channel or member.

Jobs retain the limits and platform restrictions used when submitted. Retries use
current profiles. Playlist entries inherit the parent job settings. Older watch
rules with their own limits are migrated to channel profiles.

## Media sites

Create a site under **Media sites** to share completed media at `/f/<slug>`.
Choose a profile, included Discord servers or channels, and whether downloads are
allowed. Empty server and channel lists include all media.

Sites support these access methods:

- Public access without login
- A shared PIN, password or access token
- Separate site accounts
- Configured login providers

Discord login can require membership in all selected servers or restrict access
to listed users. Viewer sessions last 30 days. Admins can manage site accounts
and end sessions from the site list.

Enable **Discord links** to post a media page when an upload exceeds the size
limit or falls below the configured quality thresholds. These links require
`web.public_url`. The media page includes preview metadata for Discord.

Signed media links work without login until they expire, after 30 days by default.
Anyone with the link can access that file until expiry. When several sites match,
a channel match takes priority over a server match, followed by unrestricted sites.
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

## Monitoring

- **Health** checks the database, engine, storage, ffmpeg, bots and platform tests.
- **Metrics** shows resource use, job counts, requests and storage, refreshed every five seconds.
- **Server log** shows up to 5,000 retained lines with filters and live updates.
- **Audit log** records who changed settings, applications, rules, profiles and access.

Audit entries are stored with the associated changes. Secrets are redacted.
Admins can filter and page through entries in the app or at `GET /api/audit`.
