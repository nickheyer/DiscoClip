![image](https://user-images.githubusercontent.com/60236014/215372009-d6ca97db-f187-4c39-a8d9-d7ac31e5d52a.png)

# DiscoClip
A web-app/bot that monitors discord channels for video-links then downloads, transcodes, posts, and archives each video.

## Running

```
DISCOCLIP_CONFIG=discoclip.toml discoclip

# or

discoclip --config discoclip.toml
```

## Building

The web app in `crates/app/ui` is a SvelteKit app that `cargo build` compiles with Node.js
and npm and embeds in the binary, so the binary is the whole deployment. While working on
the web app, `npm run dev` in `crates/app/ui` serves it with hot reload and proxies `/api`
to a running server at `127.0.0.1:8080` (`DISCOCLIP_API` names another).

## First run

Until an account exists the server prints a setup token at startup. Open `/setup` in the
web app and enter it to create the admin account. Logins are rate limited, sessions live
in the database and can be reviewed and ended from the web app, and every state-changing
request must carry the session's CSRF token.

Accounts can also log in through GitHub, Google or any OpenID Connect issuer configured
under `[auth]`, and through Discord once a Discord application is marked for login;
identities are linked and unlinked from the account page, and the providers' tokens are
kept encrypted under `secret.key` in the data directory.

## Serving

`web.bind` is where the app listens. With `[web.tls]` naming a certificate and key in
PEM the binary serves HTTPS itself, and re-reads the files whenever they change, so a
renewed certificate takes effect without a restart. Without it the app speaks plain HTTP,
which is how it runs behind a reverse proxy that terminates TLS.

The web app watches jobs and bots through one live event stream per browser, shared by
its tabs, so a browser's few connections to the host stay free for pages, downloads and
video playback however many tabs are open. A proxy in front of the app must pass
`text/event-stream` responses through unbuffered.

Behind a proxy, list the proxy's addresses or networks in `web.trusted_proxies`. Requests
that arrive from one of them are read for `Forwarded`, `X-Forwarded-For`,
`X-Forwarded-Proto` and `X-Forwarded-Host`, so sessions, rate limits, the audit log, the
origin check and login callbacks see the browser and the public scheme and host rather
than the proxy. Requests from anywhere else keep the address they arrived from, whatever
headers they carry. Session cookies are marked `Secure` whenever the browser reached the
app over HTTPS, by the binary or through a trusted proxy.

## Discord

Discord applications are added in the web app with their bot token and, for login, their
client secret; both are stored encrypted. Every application runs its own bot, and several
applications can run side by side. Each application page offers the link that adds its bot
to a guild, with the scopes and permissions it asks for, and lists the guilds the bot is in
and the ones it was removed from.

Which channels a bot watches is set by rules, one per channel: where results are posted,
which link hosts count, which users or roles may post them, and how big or long a video
may be. Operators edit any guild's rules; anyone who manages a guild on Discord and has
linked their Discord account edits that guild's rules. A guild's page lists the channels
the bot sees, marks the watched ones and starts a rule from any of them; a rule's channels,
roles and members are picked by name through the bot, with members found by searching.

The `/clip` and `/status` slash commands are registered per application from its page:
globally, in chosen guilds, or not at all. Each bot is started, stopped and restarted from
the app; a stopped bot stays stopped until started again, and every bot's state streams
live to the app.

Scripts use API tokens minted from the account page, sent as `Authorization: Bearer dc_...`;
each token is limited to the permissions it was given and can be revoked at any time.

## Platforms

The platforms page lists every resolver: the hosts it takes links from, the kinds of links
it handles, the formats the media arrives in, and whether a logged-in session is needed.
Each platform names public fixture links; running them resolves every link without
downloading anything and records what came of it, so the page shows which platforms work
right now, which link fails and why, and when each platform last passed in full. The
fixtures run on their own at `fixtures.interval_secs`, daily by default, and from the page
by anyone who manages jobs; `fixtures.timeout_secs` bounds one link.

YouTube links of every shape are taken: videos, shorts, live streams, premieres once they
start, playlists, channels and clips, with the portion a clip or a `t=` timestamp names.
The player script, several megabytes of obfuscated code, runs whole in the JavaScript
interpreter, where the player's own URL builder applies the signature cipher and the
throttling transform to every format URL; it stays loaded between links and is let go
after ten idle minutes. Age-gated videos go through the embedded player and, when that is
refused, need a logged-in session; a premiere that has not started yet is reported with
its start time.

Twitter and X posts are read through the site's own API as a guest or a session, with
the fxtwitter mirror behind it. TikTok videos come from the page data the web app
renders, with `vm.tiktok.com` and `/t/` short links unwrapped. Instagram posts, reels and
IGTV come through the web app's query or the embed page, and a post carrying several
videos becomes a playlist of them. Facebook videos, reels and watch links come from the
data the page hands its player, with `fb.watch` and `/share/` links unwrapped.

Twitch recordings, highlights, clips and live channels go through the GQL API the site
uses, with the client id read from the site whenever GQL stops taking the known one, and
subscriber-only recordings asking for a session. Kick live channels, recordings and clips
come from the site's API. Vimeo videos, unlisted links, embeds, custom links and live
events come from the configuration the player reads, with showcases, channels, groups
and user pages as playlists; a video Vimeo locks with DRM is reported as such. Dailymotion
videos and live streams come from the player's metadata, with `dai.ly` links, playlists
and user pages, and the family filter turned off for age-gated videos. Streamable videos,
Imgur videos, animated images, galleries and albums, and Redgifs clips come from the APIs
their players read.

Bilibili videos, multi-part videos (each part a job, or one part by `p=`), bangumi episodes
and seasons, favourites folders, collections, series and user spaces come through the web
API the site itself uses, carrying the device cookie it hands out and the WBI signature it
checks, with `b23.tv` short links unwrapped; videos locked to a region or to paying members
are reported as such, and the site's JSON subtitles are fetched. Niconico videos come from
the watch page's player data and the HLS access rights the delivery API grants, with
`nico.ms` links, mylists, series and user pages; premium-only videos ask for a session.
Douyin videos come through the web API when a browser's cookies are stored and from the
share page the app renders for visitors otherwise, with `v.douyin.com` links unwrapped;
Kuaishou videos come from the state the desktop page renders, then the mobile share
page's, with `v.kuaishou.com` links, and the site's request that an unfamiliar client
verify itself is reported so a browser's session can be stored. Weibo posts, reposts and
Weibo TV shows come through the site's API with the visitor cookies it hands anonymous
browsers, and `t.cn` links are unwrapped. Xiaohongshu video notes come from the state the
note page renders, with `xhslink.com` share links followed hop by hop to the note they name
and their `xsec_token` carried over; the token opens the note to visitors while it lasts,
a note the site cannot serve is read off the explore page it falls back to, and a session
unlocks the rest. VK videos,
clips, embeds (`video_ext.php`) and live streams come through the player request the
site's pages make, after a visit for the visitor cookies it wants, with MP4 files by
height and HLS and DASH manifests.

Rumble videos, embeds and live streams come from the embed data the player loads, with
their subtitles, and channel and user pages as playlists. Odysee videos, reposts, embeds
and `lbry://` links come through the site's own API, with channels as playlists. Bluesky
posts with video, quote posts included, come from the public AT Protocol API. Mastodon
posts and boosts come through the Mastodon API on any server of the fediverse that
speaks it, Pleroma, Akkoma and GoToSocial among them, with every attachment of a post;
a page shaped like a post on a server that does not speak it is handed on to the next
resolver. Threads posts come from the data the page renders, with carousels as playlists.
Tumblr posts and reblogs come through the site's API with the token its web app carries,
a blog's video posts as a playlist and a video hosted elsewhere handed to its host's
resolver. Pinterest video pins and idea pins come through the resource API the site's
pages call, with `pin.it` links unwrapped. LinkedIn posts and feed updates come from the
page the site serves visitors. Snapchat Spotlight snaps and public stories come from the
data the web pages render, a story of several snaps as a playlist. Loom recordings,
embeds and password-protected recordings come through the share page's API, with the
stream's CloudFront credentials kept as cookies so every segment is let through.
Telegram posts in public channels come through the embed the site renders, an album of
several videos as a playlist.

Discord attachment links on the CDN and the media proxy resolve as the files they are,
and a link to a message is read through whichever running bot can see its channel: the
message's video attachments, several of them as a playlist, or the embed of a video hosted
elsewhere handed to that host's resolver; a signed attachment link that has expired says
so. 9GAG animated posts come through the API the site's pages call, in every codec the
site keeps, with a post hosting a YouTube video handed to YouTube. iFunny videos come from
the page the site renders. Newgrounds movies come from the video sources the site hands
its player, by height, completing NG Guard's proof of work and retaining its clearance
cookie when requested. Internet Archive items come through the metadata API: each video with
its derivatives as variants, an item holding several videos as a playlist, and a link to
one file resolving that file. Wikimedia Commons and Wikipedia file pages come through the
MediaWiki API's video info, the original upload with every transcode, and a direct link to
an upload resolves the same way. Coub loops come through the API the player reads, as the
shareable MP4 with sound when the site rendered one and otherwise the silent loop paired
with its audio track. GIPHY GIFs, stickers and clips come from the page's data, the clips
with their sound in several sizes, with `gph.is` links unwrapped and direct media links
taken by their id; Tenor GIFs come from the store the view page renders, `tenor.com/….gif`
links unwrapped and direct media links probed. Catbox files are probed for their length,
and an album's video files become a playlist. Google Drive files come with the original
upload from the download endpoint and the player's streams from its video info, folders
as playlists from the embeddable folder view. Dropbox shared files resolve through their
download link, with the file's name and length. OneDrive shared files and folders come
through the share API the web app calls with the anonymous token it asks for first, a
folder's videos as a playlist. MEGA files and folders come through the API with the key
the link carries: the file's name decrypted from its attributes, a folder's nodes with
their keys, and the file's bytes decrypted with AES-128 in counter mode as they download.

Embedded players resolve through the APIs and manifests their players read. JW Player
provides its media renditions, captions and playlists through the delivery API.
Brightcove uses the policy key in the player's script to read videos, reference IDs and
playlists, with MP4, HLS, DASH, text tracks and DRM status. Wistia provides its media
assets, originals and captions, and turns playlists and channels into lists of videos.
Kaltura starts the player's widget session to read entries, ready renditions and captions.
Vidyard provides MP4 and HLS renditions with its player referer, and players with several
chapters become playlists.

Cloudflare Stream and Mux expand their HLS manifests and preserve signed playback links;
Cloudflare also offers DASH and an MP4 download when available, while Mux probes for its
static renditions. Bunny Stream reads the embed page for HLS, available MP4 fallbacks,
original uploads and captions, carrying the player's referer to the media. Web pages
recognize these players in iframes, embed scripts, metadata and inline player elements,
then hand the link to its resolver. YouTube, including its nocookie embeds, Vimeo,
Twitch recordings, clips and live channels, and Streamable embeds use their existing
platform resolvers. Recorded responses cover each new player and its routing from a page.

1News articles list the players their content carries, Brightcove videos through the
site's player and YouTube embeds: one player is handed to its resolver, several become a
playlist. 17LIVE rooms on air list their HTTP-FLV pulls from every CDN by quality, with
the streamer's caption as the title, and a room off air says so, with when its last
stream ended; clips come with their source upload and transcode, and recordings with
their HLS renditions beside the source and re-encoded MP4 files.

Platforms that read more with an account take a logged-in session: admins import a
browser's cookies for the platform from its row, as a Netscape `cookies.txt` or a `Cookie`
header, and the platform is asked whom they log in as; the check can be run again and
the cookies cleared at any time. Cookies are stored encrypted under `secret.key`, sent
with every request the platform's resolver makes, and never shown again; the audit log
records imports and clearings by count only. Each resolver also carries the consent and
age-gate cookies its platform wants, sent whenever the stored session has no say.

Every request a resolver makes goes through one HTTP client: per-platform and per-host
proxies, per-host rate limits and retries from the `http` settings, redirects followed
so short links land on the platform's own resolver, and embedded players handed to the
resolver that knows them. Player scripts that guard media with
signature ciphers run in a sandboxed JavaScript interpreter with bounds on how long they
may take.

## Health, metrics and the log

The health page checks every part the server runs on: the database, the job engine, the
cache and local publishing directories, ffmpeg, the bots and the platform fixtures, and
sums them up as healthy, needing a look, or failing. The metrics page reads the process
and the machine (memory, CPU, load, the disks holding the server's directories), the job
counts and the workers' load, the HTTP client's requests by host and status with its
retries and rate limit waits, the bots by state, the cache and database sizes, the
fixtures and the log, every five seconds. The log page shows the last 5000 lines the
server wrote, filtered by level, target and text, follows new lines as they are written,
and pages back through the kept ones; admins read it, as it needs `view_logs`.

## Accounts

Accounts hold one of three roles, admin, operator or viewer; the roles page shows what
each allows and who holds it. Admins manage accounts from the accounts page, see every
live session and every API token across accounts from the sessions and API tokens pages,
and end or revoke any of them.

## Settings

Settings live in the database under `data_dir` (default `data`) and are managed from the
web app's settings page by admins. A provisioning file, `discoclip.toml`, `.yaml`, `.yml`
or `.json` (see `discoclip.example.toml`), and `DISCOCLIP_<SECTION>__<KEY>` environment
variables seed them at startup. A value changed in the web app is kept even when the file
still names the old one.

Every setting takes effect the moment it is saved: the log filter, the engine's workers,
limits, archive, playlist, live capture and retention rules, its cache directory, the
HTTP client's user agent, timeouts, retries, per-host rate limits and proxies, local
publishing, the fixture schedule, the listener address and TLS files (the app listens
again on the new ones), the trusted proxies, the public URL and the login providers. A change the running server
cannot take, such as an address already in use or a certificate file that does not load,
is refused before anything is stored. The settings page also exports the stored values as
a provisioning file and imports one, and every change is written to the audit log.

## Audit log

Every settings change and every Discord management action is written to an audit log in
the same database transaction as the change: who did it (an account, from a browser session
or with an API token, with its address; or provisioning, at startup), when, what it was done
to, and what changed. Settings entries carry the value before and after and whatever was
stored around or beneath the key and removed by the write; application entries the fields
that changed, the command scope before and after, and how a registration went; rule
entries the rule before and after. Secrets never appear: a bot token or client secret is
logged as replaced, set or removed, and settings values at keys named like secrets are
redacted.

The actions are `settings.set`, `settings.reset`, `settings.import`,
`settings.provision`, `application.create`, `application.update`, `application.delete`,
`application.commands.set`, `application.commands.register`, `bot.start`, `bot.stop`,
`bot.restart`, `rule.create`, `rule.update`, `rule.delete`, `session.import` and
`session.clear`. Admins read the log at `GET /api/audit`, newest first, narrowed by
`actor` (an account id), `action`, `target_kind` (`setting`, `application`, `rule` or
`platform`) with `target_id`, and `since` and `until`; `limit` sets the page size and `before` takes the `next` of the previous page. An
API token needs the `view_audit_log` scope to read it.
