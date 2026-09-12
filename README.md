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
linked their Discord account edits that guild's rules.

The `/clip` and `/status` slash commands are registered per application from its page:
globally, in chosen guilds, or not at all. Each bot is started, stopped and restarted from
the app; a stopped bot stays stopped until started again, and every bot's state streams
live to the app.

Scripts use API tokens minted from the account page, sent as `Authorization: Bearer dc_...`;
each token is limited to the permissions it was given and can be revoked at any time.

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
publishing, the listener address and TLS files (the app listens again on the new ones),
the trusted proxies, the public URL and the login providers. A change the running server
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
`bot.restart`, `rule.create`, `rule.update` and `rule.delete`. Admins read the log at
`GET /api/audit`, newest first, narrowed by `actor` (an account id), `action`,
`target_kind` (`setting`, `application` or `rule`) with `target_id`, and `since` and
`until`; `limit` sets the page size and `before` takes the `next` of the previous page. An
API token needs the `view_audit_log` scope to read it.
