# API

## Conventions

### Base

| Key | Value |
|---|---|
| Prefix | `/api` |
| Request body | `application/json` |
| Response body | `application/json` |
| Error body | `{ "error": string }` |

### Authentication

| Carrier | Header |
|---|---|
| Browser session | `Cookie: discoclip_session=<token>` |
| API token | `Authorization: Bearer dc_<secret>` |
| CSRF echo | `x-csrf-token: <csrf_token>` |
| Origin check | `Origin` matches `Host` or trusted forwarded host |
| Site check | `Sec-Fetch-Site` in `same-origin` `none` |

| Method | CSRF echo | Origin check | Site check |
|---|---|---|---|
| `GET` `HEAD` `OPTIONS` | no | no | no |
| `POST` `PUT` `PATCH` `DELETE` | session only | yes | yes |

### Session cookie

| Key | Value |
|---|---|
| Name | `discoclip_session` |
| Path | `/` |
| HttpOnly | `true` |
| SameSite | `Lax` |
| Max-Age | 30 days |
| Absolute lifetime | 30 days |
| Idle lifetime | 14 days |

### Rate limits

| Key | Attempts | Window | Response |
|---|---|---|---|
| Incorrect password per username | 5 | 15 minutes | `429` |
| Incorrect password or unknown bearer per address | 20 | 15 minutes | `429` |
| Wrong recovery key per address | 5 | 15 minutes | `429` |
| Pending provider logins per server | 10000 | 10 minutes | `429` |

### Roles and permissions

| Permission | `admin` | `operator` | `viewer` |
|---|---|---|---|
| `manage_users` | yes | no | no |
| `manage_applications` | yes | no | no |
| `view_audit_log` | yes | no | no |
| `manage_settings` | yes | no | no |
| `view_logs` | yes | no | no |
| `manage_watch_rules` | yes | yes | no |
| `manage_bots` | yes | yes | no |
| `manage_jobs` | yes | yes | no |

### Global status codes

| Code | Body |
|---|---|
| `400` | `text/plain` · invalid JSON syntax |
| `401` | `{ "error": "not logged in" }` |
| `403` | `{ "error": "request origin does not match this host" }` |
| `403` | `{ "error": "cross-site request" }` |
| `403` | `{ "error": "missing or wrong CSRF token" }` |
| `403` | `{ "error": "this needs a browser session, not an API token" }` |
| `403` | `{ "error": "the <role> role does not allow <permission>" }` |
| `403` | `{ "error": "this API token was not given <permission>" }` |
| `404` | `{ "error": "not found" }` |
| `415` | `text/plain` · unsupported content type |
| `422` | `text/plain` · invalid JSON schema |
| `429` | `{ "error": "Too many attempts. Try again in <n> seconds." }` + `Retry-After: <n>` |
| `416` | `{ "error": "<message>" }` + `Content-Range: bytes */<length>` |
| `500` | `{ "error": "internal error" }` |
| `502` | `{ "error": "<message>" }` |
| `503` | `{ "error": "<message>" }` |

### Types

| Type | Wire form |
|---|---|
| `uuid` | string |
| `timestamp` | RFC 3339 string |
| `snowflake` | decimal string |
| `url` | string |
| `ip` | string |

## Setup and login

### GET /api/setup

Check whether setup is required.

| Field | Value |
|---|---|
| Auth | none |
| Response | `200` `SetupStatus` |

### POST /api/setup

Create the first admin account and sign in.

| Field | Value |
|---|---|
| Auth | none |
| Body | `SetupRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `400` `409` |
| Requires | No existing accounts |

### POST /api/recover

Reset an account password using the server recovery key.

| Field | Value |
|---|---|
| Auth | none |
| Body | `RecoverRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `400` `403` `404` `429` |
| Effects | Existing sessions revoked · lockout cleared · recovery key rotated |

### POST /api/login

Sign in with a username and password.

| Field | Value |
|---|---|
| Auth | none |
| Body | `LoginRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `401` `429` |

### POST /api/logout

End the current browser session.

| Field | Value |
|---|---|
| Auth | session |
| Response | `204` + cleared cookie |
| Errors | `401` `403` |

### GET /api/session

Get the current account and authentication details.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `WhoAmI` |
| Errors | `401` |

## Sessions

### GET /api/sessions

List your active browser sessions.

| Field | Value |
|---|---|
| Auth | session |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` |

### GET /api/sessions/all

List all active sessions.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Response | `200` `AccountSessionView[]` |
| Errors | `401` `403` |
| Order | Newest first |

### DELETE /api/sessions/others

End your other browser sessions.

| Field | Value |
|---|---|
| Auth | session |
| Response | `200` `Revoked` |
| Errors | `401` `403` |

### DELETE /api/sessions/{id}

End one of your browser sessions.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Response | `204` + cleared cookie when `id` is the current session |
| Errors | `401` `403` `404` |

## Roles

### GET /api/roles

List account roles and permissions.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Response | `200` `RoleView[]` |
| Errors | `401` `403` |

## Users

### GET /api/users

List accounts.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Response | `200` `User[]` |
| Errors | `401` `403` |

### POST /api/users

Create an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Body | `UserCreateRequest` |
| Response | `201` `User` |
| Errors | `400` `401` `403` `409` |

### GET /api/users/{id}

Get an account.

| Field | Value |
|---|---|
| Auth | self or `manage_users` |
| Path | `id` `uuid` |
| Response | `200` `User` |
| Errors | `401` `403` `404` |

### PATCH /api/users/{id}

Change an account's role.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Body | `UserUpdateRequest` |
| Response | `200` `User` |
| Errors | `401` `403` `404` `409` |

### DELETE /api/users/{id}

Delete an account and revoke its access.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

### PUT /api/users/{id}/password

Change a password and end other sessions.

| Field | Value |
|---|---|
| Auth | self with `current_password` or `manage_users` |
| Path | `id` `uuid` |
| Body | `PasswordRequest` |
| Response | `204` |
| Errors | `400` `401` `403` `404` `429` |

### GET /api/users/{id}/sessions

List an account's browser sessions.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` `404` |

### DELETE /api/users/{id}/sessions

End all of an account's browser sessions.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Response | `200` `Revoked` + cleared cookie when `id` is the requesting account |
| Errors | `401` `403` `404` |

### DELETE /api/users/{id}/sessions/{session}

End an account's browser session.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `session` `uuid` |
| Response | `204` + cleared cookie when `session` is the requesting session |
| Errors | `401` `403` `404` |

## API tokens

### GET /api/tokens

List your API tokens.

| Field | Value |
|---|---|
| Auth | session |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` |

### POST /api/tokens

Create an API token.

| Field | Value |
|---|---|
| Auth | session |
| Body | `TokenCreateRequest` |
| Response | `201` `Minted` |
| Errors | `400` `401` `403` |
| Secret | Returned once |

### GET /api/tokens/all

List all active API tokens.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Response | `200` `AccountTokenView[]` |
| Errors | `401` `403` |
| Order | Newest first |

### DELETE /api/tokens/{id}

Revoke one of your API tokens.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

### GET /api/users/{id}/tokens

List an account's API tokens.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` `404` |

### DELETE /api/users/{id}/tokens/{token}

Revoke an account's API token.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `token` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

## Login providers

### GET /api/auth/providers

List available login providers.

| Field | Value |
|---|---|
| Auth | none |
| Response | `200` `ProviderInfo[]` |

### GET /api/auth/{provider}/start

Start a provider login or account link.

| Field | Value |
|---|---|
| Auth | none for `intent=login` · any for `intent=link` |
| Path | `provider` `string` |
| Query | `intent` `Intent` default `login` |
| Response | `303` `Location: <provider authorize url>` |
| Errors | `400` `401` `404` `429` `502` |

### GET /api/auth/{provider}/callback

Complete a provider login or account link.

| Field | Value |
|---|---|
| Auth | none for `login` · session of the linking account for `link` |
| Path | `provider` `string` |
| Query | `code` `string` · `state` `string` · `error` `string` |
| Response | `303` `Location` per `CallbackRedirect` + `Set-Cookie` after a login |

### GET /api/auth/identities

List your linked identities.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `Identity[]` |
| Errors | `401` |

### DELETE /api/auth/identities/{provider}

Unlink an identity and revoke its provider grant.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Response | `200` `Unlinked` |
| Errors | `401` `404` `409` |

### POST /api/auth/identities/{provider}/refresh

Refresh a linked identity and its tokens.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Response | `200` `Identity` |
| Errors | `401` `404` `409` `502` |

## Settings

### GET /api/settings

Get current settings and defaults.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `SettingsView` |
| Errors | `401` `403` |

### PATCH /api/settings

Apply multiple settings changes.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Body | `SettingsChange` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

### PUT /api/settings/{key}

Set a value by dotted key.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Body | `SettingSetRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |
| Secret values | `null` preserves saved secrets at any nesting level |

### DELETE /api/settings/{key}

Reset a key and its children to defaults.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

### POST /api/settings/import

Import settings from a config file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Body | `SettingsImportRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

### GET /api/settings/export

Export settings as a config file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Query | `format` `SettingsFormat` default `toml` |
| Response | `200` `application/toml` `application/yaml` `application/json` file |
| Errors | `400` `401` `403` |

## Discord applications

### GET /api/discord/applications

List Discord applications.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Response | `200` `ApplicationView[]` |
| Errors | `401` `403` |

### POST /api/discord/applications

Add a Discord application and start its bot.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Body | `ApplicationCreateRequest` |
| Response | `201` `ApplicationView` |
| Errors | `400` `401` `403` `409` `502` |

### GET /api/discord/applications/{id}

Get a Discord application.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` |

### PATCH /api/discord/applications/{id}

Update a Discord application.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Body | `ApplicationUpdateRequest` |
| Response | `200` `ApplicationView` |
| Errors | `400` `401` `403` `404` `502` |

### DELETE /api/discord/applications/{id}

Stop the bot and delete its application.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |
| Also deleted | Watch rules and server records |

### GET /api/discord/applications/{id}/install

Get a bot invite link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | `guild` `snowflake` |
| Response | `200` `InstallLink` |
| Errors | `401` `403` `404` |

### GET /api/discord/applications/{id}/guilds

List servers the bot has joined.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Response | `200` `BotGuild[]` |
| Errors | `401` `403` `404` |

### GET /api/discord/applications/{id}/guilds/{guild}/channels

List visible channels and watch rules.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Response | `200` `GuildChannel[]` |
| Errors | `401` `403` `404` `409` `502` |

### GET /api/discord/applications/{id}/guilds/{guild}/roles

List visible server roles.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Response | `200` `GuildRole[]` |
| Errors | `401` `403` `404` `409` `502` |

### GET /api/discord/applications/{id}/guilds/{guild}/members

Search server members by name.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | `q` `string` · `limit` `integer` default `20` max `100` |
| Response | `200` `GuildMember[]` |
| Errors | `400` `401` `403` `404` `409` `502` |

### GET /api/discord/applications/{id}/guilds/{guild}/members/{user}

Get a server member.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` · `user` `snowflake` |
| Response | `200` `GuildMember` |
| Errors | `400` `401` `403` `404` `409` `502` |

## Slash command registration

### GET /api/discord/applications/{id}/commands

Get command registration status.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` |

### PUT /api/discord/applications/{id}/commands

Set the registration scope and register commands.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Body | `CommandScope` |
| Response | `200` `CommandsView` |
| Errors | `400` `401` `403` `404` `409` `502` |

### POST /api/discord/applications/{id}/commands/register

Register commands using the saved scope.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` `409` `502` |

## Bots

### POST /api/discord/applications/{id}/bot/start

Enable and start the bot.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

### POST /api/discord/applications/{id}/bot/stop

Stop the bot until manually started.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

### POST /api/discord/applications/{id}/bot/restart

Enable and restart the bot.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

### GET /api/discord/bots/events

Stream bot states and updates.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `text/event-stream` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

## Watch rules

| Rule | Scope |
|---|---|
| `channel_id: null` | Entire server |
| Channel rule | Overrides the server rule |
| Disabled channel rule | Excludes the channel |

### GET /api/discord/applications/{id}/guilds/{guild}/rules

List server watch rules.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Response | `200` `Rule[]` |
| Errors | `401` `403` `404` |

### POST /api/discord/applications/{id}/guilds/{guild}/rules

Create a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Body | `RuleInput` |
| Response | `201` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |
| Conflict | `409` for an existing channel or server rule |

### GET /api/discord/rules

List watch rules across every application, with their servers and channels named.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` |
| Response | `200` `RuleView[]` |
| Errors | `401` `403` |

### GET /api/discord/rules/{id}

Get a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Response | `200` `Rule` |
| Errors | `401` `403` `404` |

### PUT /api/discord/rules/{id}

Update a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Body | `RuleInput` |
| Response | `200` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |

### DELETE /api/discord/rules/{id}

Delete a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

## Profiles

### GET /api/profiles

List profiles.

| Field | Value |
|---|---|
| Auth | any account |
| Response | `200` `Profile[]` |
| Errors | `401` |

### POST /api/profiles

Create a profile.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Body | `ProfileInput` |
| Response | `201` `Profile` |
| Errors | `400` `401` `403` `409` |

### GET /api/profiles/{id}

Get a profile.

| Field | Value |
|---|---|
| Auth | any account |
| Path | `id` `uuid` |
| Response | `200` `Profile` |
| Errors | `401` `404` |

### PUT /api/profiles/{id}

Update a profile.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `ProfileInput` |
| Response | `200` `Profile` |
| Errors | `400` `401` `403` `404` `409` |

### DELETE /api/profiles/{id}

Delete a profile and its assignments.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` `409` |
| Conflict | `409` for the built-in profile or global default |

### GET /api/profiles/presets

List platform category presets.

| Field | Value |
|---|---|
| Auth | any account |
| Response | `200` `Preset[]` |
| Errors | `401` |

### GET /api/profiles/assignments

List profile assignments.

| Field | Value |
|---|---|
| Auth | with `guild`: `manage_watch_rules` or session managing `guild`. Without: `manage_watch_rules` |
| Query | `guild` `snowflake` optional |
| Response | `200` `Assignment[]` |
| Errors | `401` `403` |
| Filter | `guild`: global and matching server assignments · omitted: all |

### PUT /api/profiles/assignments/{scope}

Assign a profile to a scope.

| Field | Value |
|---|---|
| Auth | `global`: `manage_settings`. A guild's scopes: `manage_watch_rules` or session managing the guild |
| Path | `scope` `ScopeKey` |
| Body | `{ "profile_id": uuid }` |
| Response | `200` `Assignment` |
| Errors | `400` `401` `403` `404` |

### DELETE /api/profiles/assignments/{scope}

Remove an assignment to restore inheritance.

| Field | Value |
|---|---|
| Auth | as `PUT` |
| Path | `scope` `ScopeKey` |
| Response | `204` |
| Errors | `400` `401` `403` `409` |
| Conflict | `409` for `global` |

### GET /api/profiles/effective

Get effective profile settings.

| Field | Value |
|---|---|
| Auth | any account |
| Query | `guild` `snowflake` optional · `channel` `snowflake` optional · `user` `snowflake` optional |
| Response | `200` `EffectiveView` |
| Errors | `400` `401` |
| Default scope | Global when `guild` is omitted |

## Content views

### GET /api/frontends

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `Frontend[]` |
| Errors | `401` `403` |

### POST /api/frontends

Create a content view.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Body | `FrontendInput` |
| Response | `201` `Frontend` |
| Errors | `400` `401` `403` `409` |
| Discord links | Requires `web.public_url` |

### GET /api/frontends/{id}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `Frontend` |
| Errors | `401` `403` `404` |

### PUT /api/frontends/{id}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `FrontendInput` |
| Response | `200` `Frontend` |
| Errors | `400` `401` `403` `404` `409` |

### DELETE /api/frontends/{id}

Delete a content view and its accounts.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |
| Also deleted | Viewer sessions |

### PUT /api/frontends/{id}/secret

Set or remove the shared secret.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `{ "secret": string \| null }` |
| Response | `200` `Frontend` |
| Errors | `400` `401` `403` `404` |
| Storage | Hashed · `null` removes the secret |

### GET /api/frontends/{id}/users

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `FrontendUser[]` |
| Errors | `401` `403` `404` |

### POST /api/frontends/{id}/users

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `{ "username": string, "password": string }` |
| Response | `201` `FrontendUser` |
| Errors | `400` `401` `403` `404` `409` |

### PUT /api/frontends/{id}/users/{user}/password

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `user` `uuid` |
| Body | `{ "password": string }` |
| Response | `200` `FrontendUser` |
| Errors | `400` `401` `403` `404` |

### DELETE /api/frontends/{id}/users/{user}

Delete a view account and its sessions.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `user` `uuid` |
| Response | `204` |
| Errors | `400` `401` `403` `404` |

### GET /api/frontends/{id}/sessions

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `ViewerSession[]` |
| Errors | `401` `403` `404` |

### DELETE /api/frontends/{id}/sessions

End all sessions for a view.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

### DELETE /api/frontends/{id}/sessions/{session}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `session` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

## Content view visitors

| Field | Value |
|---|---|
| Session cookie | `dcf_<slug>` |
| Media errors | `401` login required · `404` view missing or disabled |

### GET /api/f/{slug}

Get view details and login methods.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Response | `200` `FrontInfo` |
| Errors | `404` |

### POST /api/f/{slug}/login

Sign in with a shared secret or view account.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Body | `{ "secret": string }` or `{ "username": string, "password": string }` |
| Response | `200` `FrontInfo` |
| Errors | `400` `401` `404` `429` |
| Cookie | `dcf_<slug>` |
| Rate limit | Per address |

### POST /api/f/{slug}/logout

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Response | `204` |
| Errors | `404` |

### GET /api/f/{slug}/auth/{provider}/start

Start a provider login for a content view.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` · `provider` |
| Response | `303` to the provider |
| Errors | `400` `404` `429` |
| Success redirect | `/f/<slug>` |
| Failure redirect | `/f/<slug>/login?error=<reason>` |
| Reasons | `state` · `denied` · `provider` · `exchange` · `identity` · `frontend` · `not_listed` · `not_member` · `guilds` · `channels` |
| `channels` | Membership required but no running bot identifies a scoped channel's server |

### GET /api/f/{slug}/jobs

List a view's media.

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` |
| Query | `q` · `media` `MediaKind` · `resolver` · `before` `timestamp` · `limit` (at most 48) |
| Response | `200` `FrontPage` |
| Errors | `401` `404` |
| Order | Newest first |

### GET /api/f/{slug}/jobs/{id}

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` · `id` `uuid` |
| Response | `200` `FrontJob` |
| Errors | `401` `404` |

### GET /api/f/{slug}/jobs/{id}/media

Stream a media file.

| Field | Value |
|---|---|
| Auth | viewer, unless open, or a valid `t` |
| Path | `slug` · `id` `uuid` |
| Query | `t` optional |
| Response | `200` or `206` the file |
| Errors | `401` `404` `416` |
| Token | Signed `t` from `FrontJob.media_url` · valid until expiry |

### GET /api/f/{slug}/jobs/{id}/thumbnail

Serve the still that stands for the media: a frame of a video, the picture scaled down, or the cover art of sound. Made with the job, or on first request from an output that predates stills.

| Field | Value |
|---|---|
| Auth | viewer, unless open, or a valid `t` |
| Path | `slug` · `id` `uuid` |
| Query | `t` optional |
| Response | `200` JPEG |
| Errors | `401` `404` |
| Token | Signed `t` from `FrontJob.thumbnail` · the media token |
| Cache | `private, max-age=86400` |

### GET /api/f/{slug}/jobs/{id}/oembed

The oEmbed document of a media page, which link unfurlers such as Discord read for the provider and author lines of a preview. The page links to it from its head.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` · `id` `uuid` |
| Response | `200` `OEmbed` |
| Errors | `404` |

### GET /api/f/{slug}/jobs/{id}/download

Download a media file.

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` · `id` `uuid` |
| Response | `200` or `206` the file |
| Errors | `401` `403` `404` `416` |
| Requires | Downloads enabled on the view |

### GET /f/{slug}/j/{id}

Serve a media page with social previews.

| Field | Value |
|---|---|
| Preview metadata | Open Graph and Twitter |
| Preview media | Signed video/audio/image links |

## Account guilds

### GET /api/discord/guilds

List your cached Discord servers.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `Guild[]` |
| Errors | `401` |

### POST /api/discord/guilds/refresh

Refresh your Discord server list.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `Guild[]` |
| Errors | `401` `404` `502` |

### GET /api/discord/guilds/{guild}/applications

List applications whose bots have joined a server.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `guild` `snowflake` |
| Response | `200` `GuildApplication[]` |
| Errors | `401` `403` |

## Jobs

### GET /api/jobs

List jobs.

| Field | Value |
|---|---|
| Auth | any |
| Query | `JobQuery` |
| Response | `200` `JobPage` |
| Errors | `400` `401` |
| Default order | Newest first |

### POST /api/jobs

Queue a media URL for local download.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Body | `SubmitRequest` |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `503` |

### POST /api/jobs/bulk

Apply an action to multiple jobs.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Body | `BulkRequest` |
| Response | `200` `BulkResponse` |
| Errors | `400` `401` `403` |

### GET /api/jobs/stats

Get job statistics.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `JobStats` |
| Errors | `401` |

### GET /api/jobs/events

Stream job statistics and events.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` |
| Errors | `401` |

### GET /api/events

Stream job and bot updates.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

### GET /api/jobs/{id}

Get a job.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Response | `200` `Job` |
| Errors | `401` `404` |

### DELETE /api/jobs/{id}

Delete a finished job and its cached files.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

### POST /api/jobs/{id}/retry

Queue a new job using a finished job's request.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `404` `409` `503` |

### POST /api/jobs/{id}/cancel

Cancel a job and discard any live recording.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

### POST /api/jobs/{id}/stop

Stop a live capture and publish the recording.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` `409` |
| Conflict | `409` unless capturing a live stream |

### GET /api/jobs/{id}/download

Download a job artifact.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | `artifact` `Artifact` default `output` · `index` `integer` default `0` · `inline` `bool` default `false` |
| Response | `200` file · `206` file with `Range` |
| Errors | `401` `404` `409` `416` |
| Storage | Cache or archive |
| Live range response | `Content-Range: bytes a-b/*` |
| Live range wait | Up to 10 seconds for bytes beyond the current end |

### GET /api/jobs/{id}/thumbnail

Serve the still that stands for the job's output: a frame of a video, the picture scaled down, or the cover art of sound. Made with the job, or on first request from an output that predates stills, and kept with the job.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Response | `200` JPEG |
| Errors | `401` `404` |
| Storage | Cache or archive |
| Cache | `private, max-age=86400` |

### GET /api/jobs/{id}/children

List playlist child jobs.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Response | `200` `JobSummary[]` |
| Errors | `401` `404` |
| Order | Oldest first |

## Audit log

### GET /api/audit

List audit entries.

| Field | Value |
|---|---|
| Auth | `view_audit_log` |
| Query | `AuditQuery` |
| Response | `200` `Page` |
| Errors | `400` `401` `403` |
| Order | Newest first |

## Platforms

A platform is checked with links kept in the database: the ones its resolver ships with (`builtin`), ones added by hand (`custom`), and the newest links of jobs that finished on it (`job`, the three newest kept). A check tries the platform's links in use in turn, the one that resolved most recently first, and stops at the first that resolves. A link that fails while another resolves in the same run is switched off with the failure as `disabled_reason`; nothing is switched off when no link resolves. A platform's `health` is `working` when a link resolved at its last check or a job finished on it since, `failing` when every link tried failed, `login_required` when every link tried wanted a login, and `unknown` with nothing to go on. Profiles alone decide whether a platform's links are taken; checks only report.

### GET /api/platforms

List platforms with their check links, latest results and verdicts.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `PlatformCoverage[]` |
| Errors | `401` |

### GET /api/platforms/{id}

Get platform details and test results.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `string` |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `404` |

### POST /api/platforms/check

Start a check of every platform with a link in use.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Response | `202` `CheckStarted` |
| Errors | `400` `401` `403` `409` |
| Already running | Skipped |

### POST /api/platforms/{id}/check

Check one platform: its links in use, in turn, until one resolves.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` |
| Response | `202` `PlatformCoverage` |
| Errors | `400` `401` `403` `404` `409` |
| `400` | No link in use |

### POST /api/platforms/{id}/fixtures

Add a check link. The link must be one the platform's resolver takes.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` |
| Body | `FixtureLinkRequest` |
| Response | `201` `PlatformCoverage` |
| Errors | `400` `401` `403` `404` `409` |
| Conflict | `409` for a link the platform already checks |

### PATCH /api/platforms/{id}/fixtures/{link}

Change a check link's address, switch it on or off, or both. A new address starts the link over, in use again.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` · `link` `uuid` |
| Body | `FixtureLinkChange` · at least one field |
| Response | `200` `PlatformCoverage` |
| Errors | `400` `401` `403` `404` `409` |

### DELETE /api/platforms/{id}/fixtures/{link}

Remove a check link. A shipped link stays removed across restarts.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` · `link` `uuid` |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` |

### POST /api/platforms/{id}/fixtures/{link}/check

Check one link, in use or not. Its result is recorded; nothing is switched off over it.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` · `link` `uuid` |
| Response | `202` `PlatformCoverage` |
| Errors | `401` `403` `404` `409` |

## Platform sessions

### PUT /api/platforms/{id}/cookies

Replace cookies and check the platform session.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Body | `CookiesImport` |
| Response | `200` `SessionOutcome` |
| Errors | `400` `401` `403` `404` |

### DELETE /api/platforms/{id}/cookies

Delete a platform's cookies.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` |

### POST /api/platforms/{id}/session/check

Check the saved platform session.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` `502` |

## Health and metrics

### GET /api/health

Get server health checks.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `Health` |
| Errors | `401` |

### GET /api/metrics

Get server metrics.

| Field | Value |
|---|---|
| Auth | any |
| Response | `200` `Metrics` |
| Errors | `401` `500` |

## Backups and retention

### GET /api/backups

List backups and schedule status.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `BackupsView` |
| Errors | `401` `403` `500` |

### POST /api/backups

Create a database backup and rotate old backups.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `201` `BackupEntry` |
| Errors | `401` `403` `409` `500` |
| Schedule | Manual backups allowed when disabled |

### GET /api/backups/{name}

Download a backup.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Response | `200` `application/octet-stream` file with `Content-Disposition: attachment` |
| Errors | `401` `403` `404` `416` |

### DELETE /api/backups/{name}

Delete a backup.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Response | `204` |
| Errors | `401` `403` `404` `500` |

### POST /api/backups/{name}/restore

Restore a backup and restart the server.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Response | `202` `RestoreStatus` |
| Errors | `401` `403` `404` `409` `500` |
| Validation | Private copy of the selected backup |
| Preserved | `web` settings and backup directory |
| Sessions | Invalidated |
| Recovery | Safety backup before restore · automatic rollback on startup failure |
| During restore | Backup mutations return `409` · safety copy excluded from rotation |
| Client disconnect | Restore continues |

### GET /api/backups/restore/{id}

Get restore progress by receipt ID.

| Field | Value |
|---|---|
| Auth | possession of the restore receipt ID |
| Path | `id` `uuid` |
| Response | `200` `RestoreStatus` |
| Errors | `400` `404` |
| Receipt lifetime | Until process exit or the next restore |

### GET /api/retention

Get retention settings and sweep results.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `RetentionView` |
| Errors | `401` `403` |

### POST /api/retention/sweep

Run a retention sweep.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `SweepReport` |
| Errors | `401` `403` |

## Operations

### GET /healthz

Check server health.

| Field | Value |
|---|---|
| Auth | none |
| Response | `200` `503` `Healthz` |
| Status | `200` healthy · `503` failed or stopping |

### GET /metrics

Serve Prometheus metrics or the Metrics page.

| Field | Value |
|---|---|
| Auth | any for the exposition |
| Response | `200` `text/plain; version=0.0.4` unless `Accept` includes `text/html` |
| Errors | `401` `500` |

## Server log

### GET /api/logs

List server logs.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Query | `LogQuery` |
| Response | `200` `LogPage` |
| Errors | `400` `401` `403` |

### GET /api/logs/events

Stream matching log entries.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Query | `LogQuery` without `before` and `limit` |
| Response | `200` `text/event-stream` · `event: log` · `data: LogLine` · `event: skipped` · `data: Skipped` |
| Errors | `400` `401` `403` |

## Discord slash commands

### /clip

Download a video link and post it in the channel.

| Field | Value |
|---|---|
| Option | `url` `string` required |
| Reply | `Queued <url>` |
| Reply on refusal | `Could not queue <url>: <error>` |
| Reply on done | `Clipped <url>` |
| Reply on failure | `Could not clip <url>: <stage> failed: <message>` |
| Reply on cancel | `Cancelled <url>` |
| Ephemeral | `` `<raw>` is not an http(s) link `` |
| Ephemeral | `No resolver handles <url>` |
| Ephemeral | `This command needs a channel` |

### /status

Show job counts by status.

| Field | Value |
|---|---|
| Ephemeral | `DiscoClip <version>: <n> queued, <n> running, <n> done, <n> failed, <n> cancelled` |
| Ephemeral | `Could not read job stats: <error>` |

### Unknown command

Reject an unknown command.

| Field | Value |
|---|---|
| Ephemeral | `` Unknown command `<name>` `` |

## Request schemas

### SetupRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | yes |

### RecoverRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `key` | `string` · server console recovery key | yes |
| `password` | `string` | yes |

### LoginRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | yes |

### UserCreateRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | no |
| `role` | `Role` | yes |

### UserUpdateRequest

| Field | Type | Required |
|---|---|---|
| `role` | `Role` | yes |

### PasswordRequest

| Field | Type | Required |
|---|---|---|
| `password` | `string` | yes |
| `current_password` | `string` | self only |

### TokenCreateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `scopes` | `Permission[]` | no · default `[]` |
| `expires_in_days` | `integer` | no |

### SettingsChange

| Field | Type | Required |
|---|---|---|
| `set` | `object` of dotted key to `json` | no · default `{}` |
| `reset` | `string[]` | no · default `[]` |

### SettingSetRequest

| Field | Type | Required |
|---|---|---|
| `value` | `json` | yes |

### SettingsImportRequest

| Field | Type | Required |
|---|---|---|
| `format` | `SettingsFormat` | yes |
| `text` | `string` | yes |

### ApplicationCreateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | no · default Discord name |
| `bot_token` | `string` | yes |
| `client_secret` | `string` | no |

### ApplicationUpdateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | no |
| `bot_token` | `string` | no |
| `client_secret` | `string \| null` | no |
| `login` | `bool` | no |

### CommandScope

| Field | Type | Required |
|---|---|---|
| `mode` | `CommandMode` | yes |
| `guilds` | `snowflake[]` | no · default `[]` |

### RuleInput

| Field | Type | Required |
|---|---|---|
| `channel_id` | `snowflake \| null` | yes · `null`: entire server |
| `post_to` | `snowflake \| null` | no · default: source channel |
| `allow_users` | `snowflake[]` | no · default `[]` |
| `allow_roles` | `snowflake[]` | no · default `[]` |
| `enabled` | `bool` | no · default `true` |

### ProfileInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `description` | `string` | no |
| `platforms` | `PlatformToggles` | no |
| `limits` | `ProfileLimits` | no |
| `audio_language` | `string \| null` · language tag · `null`: inherit | no |

### ProfileLimits

| Field | Type | Required |
|---|---|---|
| `max_source_bytes` | `integer \| null` · > 0 | no |
| `max_duration_secs` | `integer \| null` · `0`: reject all media | no |
| `max_height` | `integer \| null` · > 0 | no |
| `max_capture_secs` | `integer \| null` · capture seconds > 0 | no |

### PlatformToggles

| Field | Type | Required |
|---|---|---|
| `default` | `PlatformDefault` · applies when `presets` is empty | no · default `inherit` |
| `presets` | `string[]` · nonempty: allow only platforms in selected presets | no |
| `overrides` | `object` of platform ID → `bool` · overrides presets and default | no |

### FrontendInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `slug` | `string` · `[a-z0-9-]` | yes |
| `description` | `string` | no |
| `enabled` | `bool` | no · default `true` |
| `profile_id` | `uuid` | no · default: built-in profile |
| `scope` | `ContentScope` | no · default: all jobs |
| `access` | `Access` | no · default: no access |
| `downloads` | `bool` | no · default `true` |
| `links` | `LinkPolicy` | no · default: off |

### ContentScope

| Field | Type | Required |
|---|---|---|
| `guilds` | `snowflake[]` | no |
| `channels` | `snowflake[]` | no |

| Selection | Jobs |
|---|---|
| Any listed server or channel | Included |
| Both lists empty | All |

### Access

| Field | Type | Required |
|---|---|---|
| `open` | `bool` · public access | no |
| `secret_kind` | `"pin" \| "password" \| "token" \| null` | no |
| `accounts` | `bool` · view account login | no |
| `providers` | `string[]` · provider IDs | no |
| `discord_members` | `bool` · membership required in all scoped servers including channel servers | no |
| `discord_users` | `snowflake[]` · allowed Discord users | no |

### LinkPolicy

| Field | Type | Required |
|---|---|---|
| `enabled` | `bool` | no · default `false` |
| `min_height` | `integer` · pixels | no · default `720` |
| `min_bitrate` | `integer` · bits/s | no · default `1500000` |
| `max_bytes` | `integer` · output size limit | no · default `2147483648` (2 GiB) |
| `signed_link_days` | `integer` | no · default `30` |

### SubmitRequest

| Field | Type | Required |
|---|---|---|
| `url` | `url` | yes |
| `limits` | `RequestLimits` | no |
| `options` | `RequestOptions` | no |

### BulkRequest

| Field | Type | Required |
|---|---|---|
| `action` | `BulkAction` | yes |
| `ids` | `uuid[]` | yes · 1 to 500 |

### JobQuery

| Field | Type | Required |
|---|---|---|
| `source` | `string` | no |
| `status` | `StatusKind` | no |
| `resolver` | `string` | no |
| `parent` | `uuid` | no |
| `top_level` | `bool` | no · default `false` |
| `q` | `string` | no |
| `before` | `timestamp` | no |
| `after` | `timestamp` | no |
| `limit` | `integer` | no · default `50` · max `500` |
| `offset` | `integer` | no · default `0` |
| `order` | `JobOrder` | no · default `newest` |

### CookiesImport

| Field | Type | Required |
|---|---|---|
| `format` | `CookieFormat` | yes |
| `text` | `string` | yes |
| `domain` | `string` | `header` only · default the platform's first host |

### LogQuery

| Field | Type | Required |
|---|---|---|
| `level` | `LogLevel` | no |
| `target` | `string` | no |
| `q` | `string` | no |
| `before` | `integer` | no |
| `limit` | `integer` | no · default `200` · max `1000` |

### AuditQuery

| Field | Type | Required |
|---|---|---|
| `actor` | `uuid` | no |
| `action` | `Action` | no |
| `target_kind` | `TargetKind` | no |
| `target_id` | `string` | with `target_kind` |
| `since` | `timestamp` | no |
| `until` | `timestamp` | no |
| `limit` | `integer` | no · default `50` · max `500` |
| `before` | `uuid` | no |

## Response schemas

### SetupStatus

| Field | Type |
|---|---|
| `needed` | `bool` |

### WhoAmI

| Field | Type |
|---|---|
| `user` | `User` |
| `csrf_token` | `string \| null` |
| `session` | `SessionView \| null` |
| `token` | `ApiToken \| null` |

### SessionView

| Field | Type |
|---|---|
| `id` | `uuid` |
| `created_at` | `timestamp` |
| `last_seen_at` | `timestamp` |
| `expires_at` | `timestamp` |
| `user_agent` | `string \| null` |
| `ip` | `ip \| null` |
| `current` | `bool` |

### AccountSessionView

| Field | Type |
|---|---|
| `user_id` | `uuid` |
| `username` | `string` |
| `...SessionView` | `SessionView` |

### Revoked

| Field | Type |
|---|---|
| `revoked` | `integer` |

### RoleView

| Field | Type |
|---|---|
| `role` | `Role` |
| `description` | `string` |
| `permissions` | `Permission[]` |
| `accounts` | `User[]` |

### User

| Field | Type |
|---|---|
| `id` | `uuid` |
| `username` | `string` |
| `role` | `Role` |
| `has_password` | `bool` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

### ApiToken

| Field | Type |
|---|---|
| `id` | `uuid` |
| `user_id` | `uuid` |
| `name` | `string` |
| `prefix` | `string` |
| `scopes` | `Permission[]` |
| `created_at` | `timestamp` |
| `last_used_at` | `timestamp \| null` |
| `expires_at` | `timestamp \| null` |

### AccountTokenView

| Field | Type |
|---|---|
| `username` | `string` |
| `...ApiToken` | `ApiToken` |

### Minted

| Field | Type |
|---|---|
| `token` | `ApiToken` |
| `secret` | `string` |

### ProviderInfo

| Field | Type |
|---|---|
| `id` | `string` |
| `name` | `string` |

### Identity

| Field | Type |
|---|---|
| `id` | `uuid` |
| `user_id` | `uuid` |
| `provider` | `string` |
| `subject` | `string` |
| `username` | `string \| null` |
| `display_name` | `string \| null` |
| `email` | `string \| null` |
| `scope` | `string \| null` |
| `expires_at` | `timestamp \| null` |
| `has_refresh_token` | `bool` |
| `linked_at` | `timestamp` |
| `updated_at` | `timestamp` |

### Unlinked

| Field | Type |
|---|---|
| `identity` | `Identity` |
| `revoked` | `bool` |

### CallbackRedirect

| Intent | Outcome | `Location` |
|---|---|---|
| `login` | success | `/` |
| `link` | success | `/account` |
| `login` | failure | `/login?error=<CallbackError>` |
| `link` | failure | `/account?error=<CallbackError>` |

### SettingsView

| Field | Type |
|---|---|
| `settings` | `object` |
| `defaults` | `object` |
| `exemplar` | `object` · placeholder values for all optional sections |
| `entries` | `SettingEntry[]` |
| `secrets` | `string[]` · populated secret keys |
| `secret_keys` | `string[]` · all secret keys |
| `data_dir` | `string` |
| `provisioning_file` | `string \| null` |
| `public_url` | `string \| null` · the address links and login callbacks are built on |
| `public_url_source` | `"configured" \| "learned" \| null` · `web.public_url`, or learned from operators' requests |

### SettingEntry

| Field | Type |
|---|---|
| `key` | `string` |
| `source` | `SettingSource` |
| `updated_at` | `timestamp` |

### ApplicationView

| Field | Type |
|---|---|
| `id` | `uuid` |
| `name` | `string` |
| `client_id` | `snowflake` |
| `login` | `bool` |
| `has_client_secret` | `bool` |
| `commands` | `CommandsState` |
| `enabled` | `bool` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `bot` | `BotStatus` |
| `install_url` | `url` |
| `login_callback_url` | `url` |

### CommandsState

| Field | Type |
|---|---|
| `mode` | `CommandMode` |
| `guilds` | `snowflake[]` |
| `registered_at` | `timestamp \| null` |
| `error` | `string \| null` |

### CommandsView

| Field | Type |
|---|---|
| `mode` | `CommandMode` |
| `guilds` | `snowflake[]` |
| `registered_at` | `timestamp \| null` |
| `error` | `string \| null` |
| `commands` | `CommandSummary[]` |

### CommandSummary

| Field | Type |
|---|---|
| `name` | `string` |
| `description` | `string` |

### InstallLink

| Field | Type |
|---|---|
| `url` | `url` |
| `scopes` | `string[]` |
| `permissions` | `string[]` |

### BotStatus

| Field | Type | Present for `state` |
|---|---|---|
| `state` | `BotState` | all |
| `since` | `timestamp` | all |
| `user` | `string` | `connected` |
| `error` | `string` | `retrying` `failed` |
| `attempt` | `integer` | `retrying` |
| `next_attempt_at` | `timestamp` | `retrying` |

### BotEvent

| Field | Type |
|---|---|
| `application` | `uuid` |
| `...BotStatus` | `BotStatus` |
| `removed` | `true` · present only in the final event after application removal |

### BotGuild

| Field | Type |
|---|---|
| `application_id` | `uuid` |
| `guild_id` | `snowflake` |
| `name` | `string` |
| `icon` | `string \| null` |
| `member_count` | `integer \| null` |
| `present` | `bool` |
| `joined_at` | `timestamp` |
| `left_at` | `timestamp \| null` |
| `updated_at` | `timestamp` |

### GuildChannel

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `kind` | `ChannelKind` |
| `parent_id` | `snowflake \| null` |
| `position` | `integer` |
| `rule` | `uuid \| null` |

### GuildRole

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `color` | `integer` |
| `position` | `integer` |
| `managed` | `bool` |

### GuildMember

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `username` | `string` |
| `display_name` | `string \| null` |
| `nick` | `string \| null` |
| `avatar` | `string \| null` |
| `bot` | `bool` |

### Rule

| Field | Type |
|---|---|
| `id` | `uuid` |
| `application_id` | `uuid` |
| `guild_id` | `snowflake` |
| `...RuleInput` | `RuleInput` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

### RuleView

| Field | Type |
|---|---|
| `...Rule` | `Rule` |
| `guild_name` | `string \| null` · as the bot recorded it on joining |
| `guild_icon` | `string \| null` · icon hash on Discord's CDN |
| `channel_name` | `string \| null` · while the bot sees the channel |
| `post_to_name` | `string \| null` · while the bot sees the channel |

### Profile

| Field | Type |
|---|---|
| `id` | `uuid` |
| `...ProfileInput` | `ProfileInput` |
| `builtin` | `bool` · built-in and nondeletable |
| `server_limits` | `ServerLimits` · profile caps |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

### ServerLimits

| Field | Type |
|---|---|
| `max_source_bytes` | `integer` |
| `max_duration_secs` | `integer \| null` · `null`: unlimited |
| `max_height` | `integer` |
| `max_capture_secs` | `integer` · maximum capture seconds |

### Preset

| Field | Type |
|---|---|
| `id` | `string`: `basic`, `sfw`, `nsfw`, `news`, `social`, `video`, `music`, `podcasts`, `live`, `files`, `images` or `players` |
| `label` | `string` |
| `description` | `string` |
| `platforms` | `string[]` · resolver IDs |

### Scope

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"global" \| "guild" \| "channel" \| "user"` | all |
| `guild_id` | `snowflake` | `guild` `channel` `user` |
| `channel_id` | `snowflake` | `channel` |
| `user_id` | `snowflake` | `user` |

### ScopeKey

| Scope | Format |
|---|---|
| Global | `global` |
| Server | `guild:<guild>` |
| Channel | `channel:<guild>:<channel>` |
| Member | `user:<guild>:<user>` |

### Assignment

| Field | Type |
|---|---|
| `scope` | `Scope` |
| `profile_id` | `uuid` |
| `updated_at` | `timestamp` |

### EffectiveProfile

| Field | Type |
|---|---|
| `platforms` | `object` of platform ID → `bool` |
| `limits` | `RequestLimits` · `null`: engine limit |
| `audio_language` | `string \| null` · language from the most specific assignment |
| `applied` | `Assignment[]` · global → server → channel → member |

### EffectiveView

| Field | Type |
|---|---|
| `...EffectiveProfile` | `EffectiveProfile` |
| `disabled` | `string[]` · disabled platform IDs |

### Frontend

| Field | Type |
|---|---|
| `id` | `uuid` |
| `...FrontendInput` | `FrontendInput` |
| `has_secret` | `bool` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

### FrontendUser

| Field | Type |
|---|---|
| `id` | `uuid` |
| `frontend_id` | `uuid` |
| `username` | `string` |
| `created_at` | `timestamp` |

### ViewerSession

| Field | Type |
|---|---|
| `id` | `uuid` |
| `frontend_id` | `uuid` |
| `subject` | `string`: `secret`, `account:<username>` or `provider:<id>:<subject>` |
| `display` | `string` |
| `created_at` | `timestamp` |
| `last_seen_at` | `timestamp` |
| `expires_at` | `timestamp` |
| `ip` | `string \| null` |
| `user_agent` | `string \| null` |

### FrontInfo

| Field | Type |
|---|---|
| `slug` | `string` |
| `name` | `string` |
| `description` | `string` |
| `downloads` | `bool` |
| `access` | `{ open, secret: "pin" \| "password" \| "token" \| null, accounts, providers: [{ id, name }], discord_members }` |
| `platforms` | `string[]` · resolver IDs |
| `viewer` | `{ frontend_id, subject, display } \| null` |

### FrontPage

| Field | Type |
|---|---|
| `jobs` | `FrontJob[]` |
| `next` | `timestamp \| null` · next page cursor (`before`) |

### FrontJob

| Field | Type |
|---|---|
| `id` | `uuid` |
| `title` | `string \| null` |
| `media` | `MediaKind` |
| `resolver` | `string` |
| `platform` | `string` · the resolver's display name |
| `uploader` | `string \| null` |
| `uploader_url` | `url \| null` |
| `webpage_url` | `url \| null` |
| `thumbnail` | `string \| null` · signed path to the still, see `GET /api/f/{slug}/jobs/{id}/thumbnail` |
| `duration_secs` | `number \| null` |
| `live` | `bool` · recorded live stream |
| `recording` | `bool` · recording in progress |
| `size` | `integer` |
| `width` | `integer \| null` |
| `height` | `integer \| null` |
| `content_type` | `string` |
| `published_at` | `timestamp` |
| `media_url` | `string` · signed media URL |
| `download_url` | `string \| null` |

### OEmbed

| Field | Type |
|---|---|
| `version` | `"1.0"` |
| `type` | `"link"` |
| `title` | `string` |
| `author_name` | `string` · the uploader, else the platform |
| `author_url` | `url` · the source page, when known |
| `provider_name` | `string` · the view's name |
| `provider_url` | `url` · the view |

### Guild

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `icon` | `string \| null` |
| `owner` | `bool` |
| `permissions` | `string` |
| `manageable` | `bool` |
| `fetched_at` | `timestamp` |

### GuildApplication

| Field | Type |
|---|---|
| `application_id` | `uuid` |
| `name` | `string` |
| `guild_name` | `string` |
| `present` | `bool` |

### Submitted

| Field | Type |
|---|---|
| `id` | `uuid` |

### BulkResponse

| Field | Type |
|---|---|
| `action` | `BulkAction` |
| `results` | `BulkOutcome[]` |
| `succeeded` | `integer` |
| `failed` | `integer` |

### BulkOutcome

| Field | Type |
|---|---|
| `id` | `uuid` |
| `ok` | `bool` |
| `error` | `string \| null` |
| `job` | `uuid \| null` |

### JobPage

| Field | Type |
|---|---|
| `jobs` | `JobSummary[]` |
| `total` | `integer` |
| `limit` | `integer` |
| `offset` | `integer` |

### JobSummary

| Field | Type |
|---|---|
| `id` | `uuid` |
| `url` | `url` |
| `status` | `JobStatus` |
| `source` | `string` |
| `origin` | `Origin` |
| `place` | `Place \| null` · Discord names |
| `destination` | `string \| null` |
| `submitted_by` | `string \| null` |
| `parent` | `uuid \| null` |
| `retry_of` | `uuid \| null` |
| `title` | `string \| null` |
| `resolver` | `string \| null` |
| `media` | `MediaKind` · probe → resolver → `video` |
| `uploader` | `string \| null` |
| `webpage_url` | `url \| null` |
| `thumbnail` | `string \| null` · `/api/jobs/{id}/thumbnail` once the job has an output a still can stand for |
| `duration_secs` | `number \| null` |
| `live` | `bool` |
| `recording` | `bool` · current or past live capture |

| `output_bytes` | `integer \| null` |
| `published_url` | `url \| null` |
| `published_reference` | `string \| null` |
| `children` | `integer` |
| `archived_files` | `integer` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `started_at` | `timestamp \| null` |
| `finished_at` | `timestamp \| null` |

### JobStatus

| Field | Type | Present for `status` |
|---|---|---|
| `status` | `StatusKind` | all |
| `stage` | `Stage` | `running` `failed` |
| `message` | `string` | `failed` |

### Origin

| Field | Type |
|---|---|
| `source` | `string` |
| `reference` | `string` |
| `url` | `url \| null` |
| `guild` | `snowflake \| null` |
| `channel` | `snowflake \| null` |

### Place

| Field | Type |
|---|---|
| `guild` | `PlaceGuild \| null` |
| `channel` | `PlaceChannel \| null` · source channel |
| `destination` | `PlaceChannel \| null` · redirected output channel |
| `author` | `GuildMember \| null` |

### PlaceGuild

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `icon` | `string \| null` · Discord CDN hash |

### PlaceChannel

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `kind` | `ChannelKind` |

### JobStats

| Field | Type |
|---|---|
| `counts` | `Stats` |
| `last_24h` | `Stats` |
| `utilisation` | `Utilisation` |
| `queue_depth` | `integer` |
| `active` | `uuid[]` |
| `resolvers` | `ResolverStats[]` |
| `at` | `timestamp` |

### Stats

| Field | Type |
|---|---|
| `queued` | `integer` |
| `running` | `integer` |
| `done` | `integer` |
| `failed` | `integer` |
| `cancelled` | `integer` |

### Utilisation

| Field | Type |
|---|---|
| `workers` | `integer` |
| `active` | `integer` |
| `waiting` | `integer` |

### ResolverStats

| Field | Type |
|---|---|
| `resolver` | `string` |
| `done` | `integer` |
| `failed` | `integer` |
| `last_done_at` | `timestamp \| null` |
| `last_failed_at` | `timestamp \| null` |

### JobEvent

| Field | Type | Present for `kind` |
|---|---|---|
| `job` | `uuid` | all |
| `at` | `timestamp` | all |
| `kind` | `JobEventKind` | all |
| `job_summary` | `JobSummary \| null` | all |
| `request` | `JobRequest` | `submitted` |
| `status` | `JobStatus` | `status` |
| `stage` | `Stage` | `progress` |
| `progress` | `Progress` | `progress` |
| `entry` | `LogEntry` | `log` |
| `ids` | `uuid[]` | `children` |
| `file` | `LocalFile` · live recording | `recording` |

### Progress

| Field | Type |
|---|---|
| `done` | `integer` |
| `total` | `integer \| null` |
| `bytes` | `integer \| null` · recorded bytes on disk |

### LogEntry

| Field | Type |
|---|---|
| `at` | `timestamp` |
| `stage` | `Stage \| null` |
| `message` | `string` |

### JobRequest

| Field | Type |
|---|---|
| `origin` | `Origin` |
| `url` | `url` |
| `destination` | `string \| null` |
| `limits` | `RequestLimits` |
| `options` | `RequestOptions` |
| `parent` | `uuid \| null` |
| `retry_of` | `uuid \| null` |
| `submitted_by` | `string \| null` |

### RequestLimits

| Field | Type |
|---|---|
| `max_source_bytes` | `integer \| null` |
| `max_duration_secs` | `integer \| null` |
| `max_height` | `integer \| null` |
| `max_capture_secs` | `integer \| null` · capture seconds |

### RequestOptions

| Field | Type |
|---|---|
| `clip` | `ClipRange \| null` |
| `subtitles` | `SubtitleMode` |
| `subtitle_language` | `string \| null` |
| `audio_language` | `string` · default `en` · fallback: original track |

### ClipRange

| Field | Type |
|---|---|
| `start` | `Duration` |
| `end` | `Duration \| null` |

### Duration

| Field | Type |
|---|---|
| `secs` | `integer` |
| `nanos` | `integer` |

### Job

| Field | Type |
|---|---|
| `id` | `uuid` |
| `request` | `JobRequest` |
| `place` | `Place \| null` · Discord names |
| `limits_in_force` | `LimitsInForce` · minimum of request and engine limits |
| `status` | `JobStatus` |
| `artifacts` | `Artifacts` |
| `log` | `LogEntry[]` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `started_at` | `timestamp \| null` |
| `finished_at` | `timestamp \| null` |

### LimitsInForce

| Field | Type |
|---|---|
| `max_source_bytes` | `integer` |
| `max_duration_secs` | `integer \| null` · `null`: unlimited · `0`: reject live streams |
| `max_height` | `integer` |
| `max_capture_secs` | `integer` · maximum capture seconds |

### Artifacts

| Field | Type |
|---|---|
| `resolved` | `Resolved \| null` |
| `recording` | `LocalFile \| null` · playable during capture |
| `announced` | `Published \| null` · capture announcement updated with the result |
| `source` | `LocalFile \| null` |
| `output` | `LocalFile \| null` |
| `delivery` | `"upload" \| "link"` |
| `link_reason` | `string \| null` |
| `published` | `Published \| null` |
| `archived` | `ArchiveEntry \| null` |
| `subtitles` | `LocalSubtitle[]` |
| `children` | `uuid[]` |
| `timings` | `StageTiming[]` |

### Resolved

| Field | Type |
|---|---|
| `resolver` | `string` |
| `media` | `MediaKind` |
| `id` | `string \| null` |
| `title` | `string \| null` |
| `description` | `string \| null` |
| `uploader` | `string \| null` |
| `uploader_url` | `url \| null` |
| `uploaded_at` | `timestamp \| null` |
| `duration` | `Duration \| null` |
| `thumbnail` | `url \| null` |
| `webpage_url` | `url \| null` |
| `live` | `bool` |
| `age_limit` | `integer \| null` |
| `clip` | `ClipRange \| null` |
| `subtitles` | `SubtitleTrack[]` |
| `variants` | `Variant[]` |

### Variant

| Field | Type |
|---|---|
| `url` | `url` |
| `kind` | `VariantKind` |
| `audio_url` | `url \| null` |
| `container` | `Codec \| null` |
| `video` | `Codec \| null` |
| `audio` | `Codec \| null` |
| `width` | `integer \| null` |
| `height` | `integer \| null` |
| `fps` | `number \| null` |
| `bitrate` | `integer \| null` |
| `size` | `integer \| null` |
| `duration` | `Duration \| null` |
| `headers` | `[string, string][]` |
| `format_id` | `string \| null` |
| `label` | `string \| null` |
| `language` | `string \| null` |
| `audio_track` | `string \| null` · platform track name |
| `audio_default` | `bool` · original default track |
| `audio_dubbed` | `bool` |
| `codecs` | `string \| null` |
| `video_only` | `bool` |
| `audio_only` | `bool` |
| `live` | `bool` |
| `drm` | `string \| null` |
| `cipher` | `Cipher \| null` |

### Cipher

| Field | Type | Present for `scheme` |
|---|---|---|
| `scheme` | `CipherScheme` | all |
| `key` | `integer[16]` | `aes128_ctr` |
| `nonce` | `integer[8]` | `aes128_ctr` |

### Codec

| Wire form | Meaning |
|---|---|
| `string` | Known codec or container |
| `{ "other": string }` | Other codec or container |

### SubtitleTrack

| Field | Type |
|---|---|
| `url` | `url` |
| `language` | `string` |
| `name` | `string \| null` |
| `format` | `SubtitleFormat` |
| `auto` | `bool` |
| `headers` | `[string, string][]` |

### LocalFile

| Field | Type |
|---|---|
| `path` | `string` |
| `size` | `integer` |
| `info` | `MediaInfo \| null` |

### MediaInfo

| Field | Type |
|---|---|
| `container` | `Codec` |
| `kind` | `MediaKind` · image: `video` track with `fps: null` |
| `duration` | `Duration \| null` |
| `video` | `VideoTrack \| null` |
| `audio` | `AudioTrack \| null` |
| `cover` | `AttachedPicture \| null` |
| `subtitles` | `EmbeddedSubtitle[]` |

### VideoTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `width` | `integer` |
| `height` | `integer` |
| `fps` | `number \| null` |
| `bitrate` | `integer \| null` |
| `index` | `integer` · stream index |
| `pix_fmt` | `string \| null` |
| `color` | `ColorInfo` |
| `hdr` | `HdrFormat \| null` |
| `field_order` | `FieldOrder` |
| `sample_aspect` | `[integer, integer] \| null` · nonsquare pixel aspect ratio |
| `vfr` | `bool` · variable frame rate |
| `alpha` | `bool` · transparency |
| `projection` | `Projection \| null` |
| `stereo` | `StereoLayout \| null` |
| `view` | `[number, number, number] \| null` · yaw/pitch/roll in degrees |

### ColorInfo

| Field | Type |
|---|---|
| `primaries` | `string \| null` |
| `transfer` | `string \| null` |
| `matrix` | `string \| null` |
| `range` | `string \| null` |

### HdrFormat

| Field | Type | Present for `format` |
|---|---|---|
| `format` | `pq` `hlg` `dolby_vision` | all |
| `profile` | `integer` | `dolby_vision` |

### FieldOrder

| Value | Meaning |
|---|---|
| `unknown` | Unspecified |
| `progressive` | Progressive |
| `top_first` | Top field first |
| `bottom_first` | Bottom field first |

### Projection

| Field | Type | Present for `layout` |
|---|---|---|
| `layout` | `equirectangular` `cubemap` `equi_angular_cubemap` `equirectangular_tile` | all |
| `padding` | `integer` | `cubemap` |
| `left` `top` `right` `bottom` | `number` · fraction of full sphere | `equirectangular_tile` |

### StereoLayout

| Value | Meaning |
|---|---|
| `side_by_side` | Horizontal pair |
| `top_bottom` | Vertical pair |

### AudioTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `channels` | `integer` |
| `sample_rate` | `integer` |
| `bitrate` | `integer \| null` |
| `index` | `integer` · stream index |
| `language` | `string \| null` |

### AttachedPicture

| Field | Type |
|---|---|
| `index` | `integer` |
| `width` | `integer` |
| `height` | `integer` |

### EmbeddedSubtitle

| Field | Type |
|---|---|
| `index` | `integer` |
| `codec` | `string` |
| `language` | `string \| null` |
| `name` | `string \| null` |
| `bitmap` | `bool` |
| `default` | `bool` |
| `forced` | `bool` |

### LocalSubtitle

| Field | Type |
|---|---|
| `language` | `string` |
| `name` | `string \| null` |
| `path` | `string` |
| `format` | `SubtitleFormat` |

### Published

| Field | Type |
|---|---|
| `reference` | `string` |
| `url` | `url \| null` |
| `at` | `timestamp` |

### ArchiveEntry

| Field | Type |
|---|---|
| `files` | `string[]` |
| `bytes` | `integer` |
| `at` | `timestamp` |

### StageTiming

| Field | Type |
|---|---|
| `stage` | `Stage` |
| `started_at` | `timestamp` |
| `ended_at` | `timestamp \| null` |

### Page

| Field | Type |
|---|---|
| `entries` | `Entry[]` |
| `next` | `uuid \| null` |

### Entry

| Field | Type |
|---|---|
| `id` | `uuid` |
| `at` | `timestamp` |
| `actor` | `Actor` |
| `action` | `Action` |
| `target` | `Target` |
| `details` | `AuditDetails` |

### PlatformCoverage

| Field | Type |
|---|---|
| `id` | `string` |
| `name` | `string` |
| `hosts` | `string[]` |
| `features` | `string[]` |
| `formats` | `string[]` |
| `media` | `MediaKind[]` |
| `tags` | `string[]` · preset IDs except `sfw` |
| `session` | `SessionSupport` |
| `on_by_default` | `bool` · set to false if it shouldnt be in the default list |
| `fixtures` | `FixtureResult[]` · in the order a check tries them |
| `last_run_at` | `timestamp \| null` · when a link was last run |
| `last_pass_at` | `timestamp \| null` · when a link last resolved |
| `last_job_at` | `timestamp \| null` · when a job last finished on the platform |
| `passed` | `integer` · links in use whose last run resolved |
| `failed` | `integer` · links in use whose last run failed |
| `login_required` | `integer` · links in use whose last run wanted a login |
| `health` | `PlatformHealth` |
| `running` | `bool` |
| `cookies` | `integer` · saved cookie count |
| `cookies_updated_at` | `timestamp \| null` |
| `session_check` | `SessionCheckResult \| null` |

### SessionCheckResult

| Field | Type | Present for `state` |
|---|---|---|
| `state` | `SessionState` | all |
| `account` | `string` | `logged_in` |
| `at` | `timestamp` | all |

### SessionOutcome

| Field | Type |
|---|---|
| `...PlatformCoverage` | `PlatformCoverage` |
| `check_error` | `string \| null` |

### FixtureResult

| Field | Type |
|---|---|
| `id` | `uuid` |
| `url` | `url` |
| `origin` | `LinkOrigin` |
| `enabled` | `bool` · whether checks try the link |
| `disabled_reason` | `string \| null` · what it failed with while another link resolved, when that is why it is off |
| `status` | `FixtureStatus` |
| `run_at` | `timestamp \| null` |
| `last_pass_at` | `timestamp \| null` |
| `error` | `string \| null` |
| `title` | `string \| null` |
| `found` | `Found \| null` |
| `duration_ms` | `integer \| null` |

### Found

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"media" \| "playlist"` | all |
| `media` | `MediaKind` | `media` |
| `variants` | `integer` | `media` |
| `entries` | `integer` | `playlist` |

### MediaKind

`video` (includes animated GIFs) · `audio` · `image` · `file`

### CheckStarted

| Field | Type |
|---|---|
| `platforms` | `string[]` |

### FixtureLinkRequest

| Field | Type | Required |
|---|---|---|
| `url` | `url` · http or https, taken by the platform's resolver | yes |

### FixtureLinkChange

| Field | Type | Required |
|---|---|---|
| `url` | `url` | no |
| `enabled` | `bool` | no |

### LinkOrigin

`"builtin"` · `"custom"` · `"job"`

### PlatformHealth

`"working"` · `"failing"` · `"login_required"` · `"unknown"`


### Health

| Field | Type |
|---|---|
| `status` | `HealthStatus` |
| `version` | `string` |
| `started_at` | `timestamp` |
| `uptime_secs` | `integer` |
| `at` | `timestamp` |
| `checks` | `HealthCheck[]` |

### Healthz

| Field | Type |
|---|---|
| `status` | `ok` `warn` `fail` `stopping` |
| `version` | `string` |
| `at` | `timestamp` |

### HealthCheck

| Field | Type |
|---|---|
| `name` | `string` |
| `label` | `string` |
| `status` | `HealthStatus` |
| `detail` | `string` |
| `href` | `string \| null` · the app page where the check's trouble is seen to, filtered to it |

| `name` | Check | `href` |
|---|---|---|
| `database` | SQLite query | `/backups` |
| `engine` | Worker status and load | `/jobs?status=running` |
| `cache` | Cache write access | `/settings?q=engine.cache_dir` |
| `local` | Local output write access | `/settings?q=local.dir` |
| `ffmpeg` | FFmpeg executable | `/settings?q=engine.ffmpeg` |
| `encoder` | Configured video encoders | `/settings?q=engine.transcode.encoder` |
| `fonts` | Subtitle rendering | none |
| `bots` | Bot failures/retries/missing tokens | `/applications` |
| `fixtures` | Platforms failing by their check links and finished jobs · `warn` when any fails | `/platforms?health=failing`, else `/platforms` |
| `retention` | Last retention sweep | `/settings?q=engine.retention` |
| `backups` | Last run successful · newest backup ≤ 2 × interval | `/backups` |

### Metrics

| Field | Type |
|---|---|
| `at` | `timestamp` |
| `version` | `string` |
| `started_at` | `timestamp` |
| `uptime_secs` | `integer` |
| `process` | `ProcessMetrics \| null` |
| `system` | `SystemMetrics` |
| `jobs` | `JobStats` |
| `http` | `HttpMetrics` |
| `bots` | `BotMetrics` |
| `cache` | `CacheMetrics` |
| `database` | `DatabaseMetrics` |
| `fixtures` | `FixtureMetrics` |
| `logs` | `LogMetrics` |
| `transcode` | `TranscodeMetrics` |
| `retention` | `RetentionStatus` |
| `backups` | `BackupMetrics` |

### ProcessMetrics

| Field | Type |
|---|---|
| `pid` | `integer` |
| `rss_bytes` | `integer` |
| `virtual_bytes` | `integer` |
| `cpu_percent` | `number` |
| `run_time_secs` | `integer` |

### SystemMetrics

| Field | Type |
|---|---|
| `total_memory_bytes` | `integer` |
| `available_memory_bytes` | `integer` |
| `load_average` | `[number, number, number]` |
| `cpus` | `integer` |
| `disks` | `DiskMetrics[]` |

### DiskMetrics

| Field | Type |
|---|---|
| `mount` | `string` |
| `total_bytes` | `integer` |
| `available_bytes` | `integer` |
| `holds` | `string[]` of `cache` `data` `local` `archive` |

### HttpMetrics

| Field | Type |
|---|---|
| `requests` | `RequestCount[]` |
| `retries` | `integer` |
| `rate_limit_waits` | `integer` |
| `bytes_received` | `integer` |

### RequestCount

| Field | Type |
|---|---|
| `host` | `string` |
| `status` | `integer` |
| `count` | `integer` |

### BotMetrics

| Field | Type |
|---|---|
| `applications` | `integer` |
| `by_state` | `object` of `BotState` to `integer` |

### CacheMetrics

| Field | Type |
|---|---|
| `dir` | `string` |
| `bytes` | `integer` |
| `jobs` | `integer` |

### DatabaseMetrics

| Field | Type |
|---|---|
| `path` | `string` |
| `bytes` | `integer` |

### FixtureMetrics

| Field | Type |
|---|---|
| `platforms` | `integer` |
| `with_fixtures` | `integer` · platforms with a check link in use |
| `working` | `integer` |
| `failing` | `integer` |
| `login_required` | `integer` |
| `unknown` | `integer` |
| `running` | `integer` |

### LogMetrics

| Field | Type |
|---|---|
| `buffered` | `integer` |
| `capacity` | `integer` |

### TranscodeMetrics

| Field | Type |
|---|---|
| `ffmpeg` | `string` · first line of `ffmpeg -version` |
| `source` | `embedded` `external` |
| `path` | `string \| null` · external FFmpeg path |
| `choice` | `EncoderChoice` |
| `hardware` | `EncoderChoice \| null` · excludes `auto` and `software` |
| `h264_encoder` | `string \| null` |
| `shortfall` | `string \| null` · encoder fallback reason |

### EncoderChoice

| Value | Meaning |
|---|---|
| `auto` | First working hardware encoder or software |
| `software` | Software encoders |
| `nvenc` | NVIDIA NVENC |
| `vaapi` | VA-API |
| `qsv` | Intel Quick Sync Video |
| `videotoolbox` | Apple VideoToolbox |
| `amf` | AMD AMF |
| `v4l2m2m` | Video4Linux memory-to-memory |

### BackupMetrics

| Field | Type |
|---|---|
| `enabled` | `bool` |
| `count` | `integer` |
| `bytes` | `integer` |
| `newest_at` | `timestamp \| null` |
| `last_error` | `string \| null` |
| `runs` | `integer` |

### BackupsView

| Field | Type |
|---|---|
| `enabled` | `bool` |
| `dir` | `string` |
| `interval_secs` | `integer` |
| `keep` | `integer` |
| `status` | `BackupStatus` |
| `backups` | `BackupEntry[]` · newest first |

### BackupEntry

| Field | Type |
|---|---|
| `name` | `string` · `discoclip-<UTC time>[-<unique ID>].db` |
| `bytes` | `integer` |
| `at` | `timestamp` |

### RestoreStatus

| Field | Type |
|---|---|
| `id` | `uuid` · restore receipt |
| `phase` | `stopping \| restoring \| starting \| complete \| failed` |
| `error` | `string \| null` |

### BackupStatus

| Field | Type |
|---|---|
| `last_at` | `timestamp \| null` |
| `last_bytes` | `integer \| null` |
| `last_error` | `string \| null` |
| `runs` | `integer` |

### RetentionView

| Field | Type |
|---|---|
| `config` | `RetentionConfig` |
| `status` | `RetentionStatus` |

### RetentionConfig

| Field | Type |
|---|---|
| `jobs_days` | `integer` · `0`: keep indefinitely |
| `failed_jobs_days` | `integer` · `0`: keep indefinitely |
| `cache_max_bytes` | `integer` · `0`: no trimming |
| `sweep_interval_secs` | `integer` |

### RetentionStatus

| Field | Type |
|---|---|
| `last` | `SweepReport \| null` |
| `sweeps` | `integer` |
| `jobs_removed_total` | `integer` |
| `bytes_freed_total` | `integer` |

### SweepReport

| Field | Type |
|---|---|
| `at` | `timestamp` |
| `jobs_removed` | `integer` · expired completed jobs |
| `failed_removed` | `integer` · expired failed/cancelled jobs |
| `bytes_freed` | `integer` |
| `error` | `string \| null` |

### LogPage

| Field | Type |
|---|---|
| `lines` | `LogLine[]` |
| `next` | `integer \| null` |
| `buffered` | `integer` |
| `capacity` | `integer` |
| `oldest_id` | `integer \| null` |

### LogLine

| Field | Type |
|---|---|
| `id` | `integer` |
| `at` | `timestamp` |
| `level` | `LogLevel` |
| `target` | `string` |
| `message` | `string` |
| `fields` | `object` of `string` to `string` |

### Skipped

| Field | Type |
|---|---|
| `count` | `integer` |

### Actor

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `ActorKind` | all |
| `id` | `uuid` | `user` |
| `username` | `string` | `user` |
| `via` | `Via` | `user` |
| `ip` | `ip` | `user` |
| `file` | `string \| null` | `provisioning` |

### Target

| Field | Type |
|---|---|
| `kind` | `TargetKind` |
| `id` | `string` |
| `name` | `string \| null` |

| `kind` | `id` | `name` |
|---|---|---|
| `setting` | settings key | `null` |
| `application` | application `uuid` | application name |
| `rule` | rule `uuid` | `<guild_id>/<channel_id>` |
| `platform` | platform id | `null` |

### AuditDetails

| `action` | Field | Type |
|---|---|---|
| `settings.set` `settings.reset` `settings.import` `settings.provision` | `value` | `json` |
| `settings.set` `settings.reset` `settings.import` `settings.provision` | `previous` | `json` |
| `settings.set` `settings.reset` `settings.import` `settings.provision` | `removed` | `object` |
| `application.create` | `name` | `string` |
| `application.create` | `client_id` | `snowflake` |
| `application.create` | `has_client_secret` | `bool` |
| `application.update` | `name` | `string` |
| `application.update` | `previous_name` | `string` |
| `application.update` | `bot_token` | `"replaced"` |
| `application.update` | `client_secret` | `"set" \| "removed"` |
| `application.update` | `login` | `bool` |
| `application.delete` | `name` | `string` |
| `application.delete` | `client_id` | `snowflake` |
| `application.commands.set` | `scope` | `CommandScope` |
| `application.commands.set` | `previous` | `CommandScope` |
| `application.commands.register` | `scope` | `CommandScope` |
| `application.commands.register` | `registered` | `bool` |
| `application.commands.register` | `error` | `string` |
| `bot.start` `bot.stop` `bot.restart` | `enabled` | `bool` |
| `rule.create` `rule.update` `rule.delete` | `application_id` | `uuid` |
| `rule.create` `rule.update` `rule.delete` | `guild_id` | `snowflake` |
| `rule.create` `rule.update` `rule.delete` | `rule` | `RuleInput` |
| `rule.update` | `previous` | `RuleInput` |
| `session.import` | `cookies` | `integer` |
| `session.import` | `previous_cookies` | `integer` |
| `session.import` | `format` | `CookieFormat` |
| `session.clear` | `cookies` | `integer` |
| `profile.create` `profile.update` `profile.delete` | `profile` | `ProfileInput` |
| `profile.create` | `converted_from_rule` | `object` · migration fields: `rule_id` · `application_id` · `guild_id` · `channel_id` · `allow_hosts` · `unmatched_hosts` (`web`) · `max_source_bytes` · `max_duration_secs` · `max_height` |
| `profile.update` | `previous` | `ProfileInput` |
| `profile.delete` | `assignments_removed` | `integer` |
| `profile.assign` `profile.unassign` | `scope` | `Scope` |
| `profile.assign` | `previous_profile_id` | `uuid \| null` |
| `frontend.create` `frontend.update` `frontend.delete` | `frontend` | `FrontendInput` |
| `frontend.update` | `previous` | `FrontendInput` |
| `frontend.user.create` `frontend.user.password` `frontend.user.delete` | `username` | `string` |
| `frontend.sessions.revoke` | `sessions` | `integer` |
| `frontend.sessions.revoke` | `session_id` | `uuid \| null` |
| `backup.run` | `name` | `string` |
| `backup.run` | `bytes` | `integer` |
| `backup.delete` | `name` | `string` |

## Enumerations

### Role

`admin` · `operator` · `viewer`

### Permission

`manage_users` · `manage_applications` · `manage_watch_rules` · `manage_bots` · `view_audit_log` · `manage_jobs` · `manage_settings` · `view_logs`

### Intent

`login` · `link`

### SettingSource

`provisioning` · `app`

### SettingsFormat

`toml` · `yaml` · `json`

### StatusKind

`queued` · `running` · `done` · `failed` · `cancelled`

### Stage

`resolve` · `download` · `transcode` · `publish` · `archive`

### BulkAction

`retry` · `cancel` · `stop` · keep live recordings · `delete`

### Artifact

`output` · `source` · `subtitle` · `recording` · available during capture

### JobOrder

`newest` · `oldest`

### JobEventKind

`submitted` · `status` · `progress` · `log` · `children` · `recording` · capture started · `stop` · stop requested · `deleted`

### SubtitleMode

`keep` · `burn` · `skip`

### SubtitleFormat

`vtt` · `srt` · `ttml` · `ass` · `json3` · `hls_vtt`

### VariantKind

`file` · `hls` · `dash` · `ism` · `rtmp` · `rtsp` · `rtp` · `whep` · `browser`

### CipherScheme

`aes128_ctr`

### CallbackError

`state` · `denied` · `provider` · `identity` · `exchange` · `session` · `already_linked` · `provider_linked` · `unknown_identity`

### CommandMode

`off` · `global` · `guilds`

### ChannelKind

`text` · `announcement` · `voice` · `stage` · `category` · `forum` · `media` · `thread` · `other`

### SessionSupport

`none` · `optional` · `required`

### FixtureStatus

`pass` · `fail` · `never`

### SessionState

`unsupported` · `logged_out` · `logged_in`

### CookieFormat

`netscape` · `header`

### HealthStatus

`ok` · `warn` · `fail`

### LogLevel

`trace` · `debug` · `info` · `warn` · `error`

### BotState

`disabled` · `stopped` · `starting` · `connected` · `retrying` · `failed`

### ActorKind

`user` · `provisioning`

### Via

`session` · `token`

### PlatformDefault

| Value | Meaning |
|---|---|
| `inherit` | Use parent settings |
| `enabled` | Enable unlisted platforms |
| `disabled` | Disable unlisted platforms |

### TargetKind

`setting` · `application` · `rule` · `platform` · `profile` · `frontend` · `backup`

### Action

`settings.set` · `settings.reset` · `settings.import` · `settings.provision` · `application.create` · `application.update` · `application.delete` · `application.commands.set` · `application.commands.register` · `bot.start` · `bot.stop` · `bot.restart` · `rule.create` · `rule.update` · `rule.delete` · `session.import` · `session.clear` · `profile.create` · `profile.update` · `profile.delete` · `profile.assign` · `profile.unassign` · `frontend.create` · `frontend.update` · `frontend.delete` · `frontend.secret.set` · `frontend.secret.clear` · `frontend.user.create` · `frontend.user.password` · `frontend.user.delete` · `frontend.sessions.revoke` · `backup.run` · `backup.delete`

### Install scopes

`bot` · `applications.commands`

### Install permissions

`VIEW_CHANNEL` · `SEND_MESSAGES` · `SEND_MESSAGES_IN_THREADS` · `EMBED_LINKS` · `ATTACH_FILES` · `READ_MESSAGE_HISTORY`
