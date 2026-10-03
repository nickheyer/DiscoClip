<p align="center">
  <img alt="DiscoClip logo" src="brand/mark.svg" width="300" />
  <h2 align="center">DiscoClip</h2>
</p>

Download media from Discord channels or a web app, convert it to fit upload limits,
and post or archive the result. Supports video, audio, images, playlists and live
stream recording.

## Install

Download a binary or Linux package from [Releases](https://github.com/nickheyer/DiscoClip/releases),
or use the installer on Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/nickheyer/DiscoClip/main/packaging/install.sh | sh
```

Run with a config based on [discoclip.example.toml](discoclip.example.toml):

```sh
discoclip --config discoclip.toml
```

Open `/setup` on the server to create the admin account. Linux binaries require
glibc 2.35 or newer. For a systemd service, install a `.deb` or `.rpm` package, or
run the installer as root with `--system`.

For Docker, use [compose.yaml](compose.yaml):

```sh
docker compose up -d
```

Images are available for amd64 and arm64 at `ghcr.io/nickheyer/discoclip:latest`
(CPU) and `ghcr.io/nickheyer/discoclip:gpu`. The compose file includes
`discoclip-nvidia` and `discoclip-intel-amd` services. NVIDIA requires the host
driver and Container Toolkit; Intel and AMD require access to `/dev/dri`.

## Use

1. In **Applications**, add a bot token from the Discord Developer Portal.
2. Select **Add to a server** to invite it.
3. Enable watching for a server or individual channels. Use **Options** to choose
   the output channel and who can submit links.

You can also submit links through the web app or `/clip`. Live recordings play
while they grow; **Stop** or `/clip stop` keeps the recording, while **Cancel**
deletes it.

**Profiles** set platform access and media limits. Settings inherit from the
global default through server, channel and member assignments, within server limits.

**Platforms** lists supported sites and lets admins import cookies for sites
that require a login.

**Content views** share media at `/f/<slug>`, with public access or a login requirement.
Enable **Discord links** to post a page instead of a file for media that exceeds upload or
quality limits. A posted page unfurls with an inline player for video up to 80 MB and
with a thumbnail card above that. Pages, login callbacks and link previews are built on
the address you open the web app at, which the server learns from your own requests; set
`web.public_url` to fix it. Signed media links allow access without login until they expire.

Every finished job's media is archived under `engine.archive.dir` (`data/archive` by
default, `/var/lib/discoclip/archive` for the packages and containers), filed by year and
month beside a JSON record of the job. Retention sweeps the working cache on its schedule
and leaves the archive alone. Turn the archive off or move it under **Settings**.

**Platforms** shows whether each site still works: a platform is checked with links kept in
the database, which are the ones it ships with, any you add, and the newest links of jobs
that finished on it, so real use keeps the set fresh. A check tries the links in turn and
stops at the first that resolves; a link that fails while another resolves is set aside.
Links can be run one at a time, edited and removed.


Admins manage the server, operators manage jobs and bots, and viewers have read
access. Users with linked Discord accounts can manage watch rules for servers
they manage on Discord. Create API tokens under **Account**; see [API.md](API.md)
for endpoints and authentication.

