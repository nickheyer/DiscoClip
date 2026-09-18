# API

## RULES

1. Do not include descriptions for anything other than a per-endpoint description
2. Your description may be a maximum of 16 words long and may only be a single sentence with no prose whatsoever.
3. Your description says what the intended operation is without any disclaimers.
4. There should be no reason for commas if you are correctly stating the single fact description. 
5. Keep this extremely organized and well patterned.
6. API docs may start below the following "## DOCS" header.


## DOCS

### Conventions

#### Base

| Key | Value |
|---|---|
| Prefix | `/api` |
| Request body | `application/json` |
| Response body | `application/json` |
| Error body | `{ "error": string }` |

#### Authentication

| Carrier | Header |
|---|---|
| Browser session | `Cookie: discoclip_session=<token>` |
| API token | `Authorization: Bearer dc_<secret>` |
| CSRF echo | `x-csrf-token: <csrf_token>` |
| Origin check | `Origin` equal to `Host` or the host a trusted proxy forwarded |
| Site check | `Sec-Fetch-Site` in `same-origin` `none` |

| Method | CSRF echo | Origin check | Site check |
|---|---|---|---|
| `GET` `HEAD` `OPTIONS` | no | no | no |
| `POST` `PUT` `PATCH` `DELETE` | session only | yes | yes |

#### Session cookie

| Key | Value |
|---|---|
| Name | `discoclip_session` |
| Path | `/` |
| HttpOnly | `true` |
| SameSite | `Lax` |
| Max-Age | 30 days |
| Absolute lifetime | 30 days |
| Idle lifetime | 14 days |

#### Rate limits

| Key | Attempts | Window | Response |
|---|---|---|---|
| Wrong password or wrong current password per username | 5 | 15 minutes | `429` |
| Wrong password or unknown bearer per address | 20 | 15 minutes | `429` |
| Wrong setup token per address | 5 | 15 minutes | `429` |
| Pending provider flows per server | 10000 | 10 minutes | `429` |

#### Roles and permissions

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

#### Global status codes

| Code | Body |
|---|---|
| `400` | `text/plain` axum JSON syntax rejection |
| `401` | `{ "error": "not logged in" }` |
| `403` | `{ "error": "request origin does not match this host" }` |
| `403` | `{ "error": "cross-site request" }` |
| `403` | `{ "error": "missing or wrong CSRF token" }` |
| `403` | `{ "error": "this needs a browser session, not an API token" }` |
| `403` | `{ "error": "the <role> role does not allow <permission>" }` |
| `403` | `{ "error": "this API token was not given <permission>" }` |
| `404` | `{ "error": "not found" }` |
| `415` | `text/plain` axum content type rejection |
| `422` | `text/plain` axum JSON schema rejection |
| `429` | `{ "error": "too many attempts; try again in <n> seconds" }` + `Retry-After: <n>` |
| `416` | `{ "error": "<message>" }` + `Content-Range: bytes */<length>` |
| `500` | `{ "error": "internal error" }` |
| `502` | `{ "error": "<message>" }` |
| `503` | `{ "error": "<message>" }` |

#### Types

| Type | Wire form |
|---|---|
| `uuid` | string |
| `timestamp` | RFC 3339 string |
| `snowflake` | decimal string |
| `url` | string |
| `ip` | string |

### Setup and login

#### GET /api/setup

Reports whether the first admin account still has to be created.

| Field | Value |
|---|---|
| Auth | none |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `SetupStatus` |
| Errors | — |

#### POST /api/setup

Creates the first admin account with the printed setup token and opens its session.

| Field | Value |
|---|---|
| Auth | none |
| Path | — |
| Query | — |
| Body | `SetupRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `400` `403` `409` `429` |

#### POST /api/login

Opens a browser session for a username and password.

| Field | Value |
|---|---|
| Auth | none |
| Path | — |
| Query | — |
| Body | `LoginRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `401` `429` |

#### POST /api/logout

Ends the browser session making the request and clears its cookie.

| Field | Value |
|---|---|
| Auth | session |
| Path | — |
| Query | — |
| Body | — |
| Response | `204` + cleared cookie |
| Errors | `401` `403` |

#### GET /api/session

Returns the account behind the request with its session or API token.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `WhoAmI` |
| Errors | `401` |

### Sessions

#### GET /api/sessions

Lists the live browser sessions of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` |

#### GET /api/sessions/all

Lists the live browser sessions of every account newest first.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `AccountSessionView[]` |
| Errors | `401` `403` |

#### DELETE /api/sessions/others

Ends every session of the requesting account except the current one.

| Field | Value |
|---|---|
| Auth | session |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Revoked` |
| Errors | `401` `403` |

#### DELETE /api/sessions/{id}

Ends one session of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` + cleared cookie when `id` is the current session |
| Errors | `401` `403` `404` |

### Roles

#### GET /api/roles

Lists every role with its permissions and the accounts holding it.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `RoleView[]` |
| Errors | `401` `403` |

### Users

#### GET /api/users

Lists every account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `User[]` |
| Errors | `401` `403` |

#### POST /api/users

Creates an account with a role and an optional password.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | — |
| Query | — |
| Body | `UserCreateRequest` |
| Response | `201` `User` |
| Errors | `400` `401` `403` `409` |

#### GET /api/users/{id}

Returns one account.

| Field | Value |
|---|---|
| Auth | self or `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `User` |
| Errors | `401` `403` `404` |

#### PATCH /api/users/{id}

Changes an account's role.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | `UserUpdateRequest` |
| Response | `200` `User` |
| Errors | `401` `403` `404` `409` |

#### DELETE /api/users/{id}

Removes an account together with its sessions and tokens and provider grants.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### PUT /api/users/{id}/password

Sets an account's password and ends its other sessions.

| Field | Value |
|---|---|
| Auth | self with `current_password` or `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | `PasswordRequest` |
| Response | `204` |
| Errors | `400` `401` `403` `404` `429` |

#### GET /api/users/{id}/sessions

Lists the live browser sessions of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/sessions

Ends every browser session of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `Revoked` + cleared cookie when `id` is the requesting account |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/sessions/{session}

Ends one browser session of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `session` `uuid` |
| Query | — |
| Body | — |
| Response | `204` + cleared cookie when `session` is the requesting session |
| Errors | `401` `403` `404` |

### API tokens

#### GET /api/tokens

Lists the API tokens of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` |

#### POST /api/tokens

Mints an API token and returns its secret once.

| Field | Value |
|---|---|
| Auth | session |
| Path | — |
| Query | — |
| Body | `TokenCreateRequest` |
| Response | `201` `Minted` |
| Errors | `400` `401` `403` |

#### GET /api/tokens/all

Lists the live API tokens of every account newest first.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `AccountTokenView[]` |
| Errors | `401` `403` |

#### DELETE /api/tokens/{id}

Revokes one API token of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` |

#### GET /api/users/{id}/tokens

Lists the API tokens of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/tokens/{token}

Revokes one API token of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `token` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` |

### Login providers

#### GET /api/auth/providers

Lists the login providers the server offers.

| Field | Value |
|---|---|
| Auth | none |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `ProviderInfo[]` |
| Errors | — |

#### GET /api/auth/{provider}/start

Redirects the browser to a provider to log in or link an identity.

| Field | Value |
|---|---|
| Auth | none for `intent=login` · any for `intent=link` |
| Path | `provider` `string` |
| Query | `intent` `Intent` default `login` |
| Body | — |
| Response | `303` `Location: <provider authorize url>` |
| Errors | `400` `401` `404` `429` `502` |

#### GET /api/auth/{provider}/callback

Completes a provider flow and redirects the browser into the app.

| Field | Value |
|---|---|
| Auth | none for `login` · session of the linking account for `link` |
| Path | `provider` `string` |
| Query | `code` `string` · `state` `string` · `error` `string` |
| Body | — |
| Response | `303` `Location` per `CallbackRedirect` + `Set-Cookie` after a login |
| Errors | — |

#### GET /api/auth/identities

Lists the provider identities linked to the requesting account.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Identity[]` |
| Errors | `401` |

#### DELETE /api/auth/identities/{provider}

Unlinks a provider identity and revokes its grant at the provider.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Query | — |
| Body | — |
| Response | `200` `Unlinked` |
| Errors | `401` `404` `409` |

#### POST /api/auth/identities/{provider}/refresh

Fetches a linked identity's profile again and renews its provider tokens.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Query | — |
| Body | — |
| Response | `200` `Identity` |
| Errors | `401` `404` `409` `502` |

### Settings

#### GET /api/settings

Returns every setting with its default and which values are stored and by whom.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `SettingsView` |
| Errors | `401` `403` |

#### PATCH /api/settings

Stores several keys and resets several keys together and applies them to the running server.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | — |
| Query | — |
| Body | `SettingsChange` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### PUT /api/settings/{key}

Stores one value at a dotted key and applies it to the running server.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Query | — |
| Body | `SettingSetRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### DELETE /api/settings/{key}

Removes the stored value at a dotted key and everything beneath it so the default applies.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Query | — |
| Body | — |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### POST /api/settings/import

Stores every key of a provisioning file as an app change and applies them.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | — |
| Query | — |
| Body | `SettingsImportRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### GET /api/settings/export

Returns the stored values as a provisioning file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | — |
| Query | `format` `SettingsFormat` default `toml` |
| Body | — |
| Response | `200` `application/toml` `application/yaml` `application/json` file |
| Errors | `400` `401` `403` |

### Discord applications

#### GET /api/discord/applications

Lists every Discord application with its bot state and install link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `ApplicationView[]` |
| Errors | `401` `403` |

#### POST /api/discord/applications

Adds a Discord application by its bot token and launches its bot.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | — |
| Query | — |
| Body | `ApplicationCreateRequest` |
| Response | `201` `ApplicationView` |
| Errors | `400` `401` `403` `409` `502` |

#### GET /api/discord/applications/{id}

Returns one Discord application with its bot state and install link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` |

#### PATCH /api/discord/applications/{id}

Changes an application's name or credentials or login flag.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | `ApplicationUpdateRequest` |
| Response | `200` `ApplicationView` |
| Errors | `400` `401` `403` `404` `502` |

#### DELETE /api/discord/applications/{id}

Removes an application with its rules and guilds and retires its bot.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/install

Returns the link that adds the application's bot to a guild.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | `guild` `snowflake` |
| Body | — |
| Response | `200` `InstallLink` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/guilds

Lists the guilds the application's bot is in or was removed from.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `BotGuild[]` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/guilds/{guild}/channels

Lists the channels of a guild as the application's bot sees them with the rule watching each.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | — |
| Body | — |
| Response | `200` `GuildChannel[]` |
| Errors | `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/roles

Lists the roles of a guild as the application's bot sees them.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | — |
| Body | — |
| Response | `200` `GuildRole[]` |
| Errors | `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/members

Searches the members of a guild by name through the application's bot.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | `q` `string` · `limit` `integer` default `20` max `100` |
| Body | — |
| Response | `200` `GuildMember[]` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/members/{user}

Returns one member of a guild by id through the application's bot.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` · `user` `snowflake` |
| Query | — |
| Body | — |
| Response | `200` `GuildMember` |
| Errors | `400` `401` `403` `404` `409` `502` |

### Slash command registration

#### GET /api/discord/applications/{id}/commands

Returns the command scope and the last registration outcome and the commands.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` |

#### PUT /api/discord/applications/{id}/commands

Sets where the slash commands are registered and registers them there.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | `CommandScope` |
| Response | `200` `CommandsView` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### POST /api/discord/applications/{id}/commands/register

Registers the slash commands again where the scope says.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` `409` `502` |

### Bots

#### POST /api/discord/applications/{id}/bot/start

Starts the application's bot and marks it meant to run.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### POST /api/discord/applications/{id}/bot/stop

Stops the application's bot until it is started again.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### POST /api/discord/applications/{id}/bot/restart

Stops and starts the application's bot and marks it meant to run.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### GET /api/discord/bots/events

Streams every bot's status now and each change as server-sent events.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `text/event-stream` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

### Watch rules

#### GET /api/discord/applications/{id}/guilds/{guild}/rules

Lists the watch rules of a guild for an application.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | — |
| Body | — |
| Response | `200` `Rule[]` |
| Errors | `401` `403` `404` |

#### POST /api/discord/applications/{id}/guilds/{guild}/rules

Adds a watch rule for a channel of a guild.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | — |
| Body | `RuleInput` |
| Response | `201` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### GET /api/discord/rules

Lists every watch rule of every application.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Rule[]` |
| Errors | `401` `403` |

#### GET /api/discord/rules/{id}

Returns one watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `Rule` |
| Errors | `401` `403` `404` |

#### PUT /api/discord/rules/{id}

Replaces what a watch rule says.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | — |
| Body | `RuleInput` |
| Response | `200` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### DELETE /api/discord/rules/{id}

Removes a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` |

### Profiles

Profiles say which platforms are on. They are put in force at scopes, addressed as
`global`, `guild:<guild>`, `channel:<guild>:<channel>` or `user:<guild>:<user>`, and
apply from the widest to the narrowest. See `Profile`, `Scope` and `EffectiveProfile`.

#### GET /api/profiles

Lists every profile.

| Field | Value |
|---|---|
| Auth | any account |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Profile[]` |
| Errors | `401` |

#### POST /api/profiles

Adds a profile.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | — |
| Query | — |
| Body | `ProfileInput` |
| Response | `201` `Profile` |
| Errors | `400` `401` `403` `409` |

#### GET /api/profiles/{id}

Returns one profile.

| Field | Value |
|---|---|
| Auth | any account |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `Profile` |
| Errors | `401` `404` |

#### PUT /api/profiles/{id}

Replaces what a profile says.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Query | — |
| Body | `ProfileInput` |
| Response | `200` `Profile` |
| Errors | `400` `401` `403` `404` `409` |

#### DELETE /api/profiles/{id}

Removes a profile and takes it off every scope; the built-in profile and the one in force
for the whole server are refused with `409`.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### GET /api/profiles/presets

The presets a profile can choose, each with the platforms in it.

| Field | Value |
|---|---|
| Auth | any account |
| Response | `200` `Preset[]` |
| Errors | `401` |

#### GET /api/profiles/assignments

The profiles in force: the whole server's and, with `guild`, that guild's scopes; every
guild's without.

| Field | Value |
|---|---|
| Auth | with `guild`: `manage_watch_rules` or session managing `guild`; without: `manage_watch_rules` |
| Path | — |
| Query | `guild` `snowflake` optional |
| Body | — |
| Response | `200` `Assignment[]` |
| Errors | `401` `403` |

#### PUT /api/profiles/assignments/{scope}

Puts a profile in force at a scope, replacing whatever was.

| Field | Value |
|---|---|
| Auth | `global`: `manage_settings`; a guild's scopes: `manage_watch_rules` or session managing the guild |
| Path | `scope` `ScopeKey` |
| Query | — |
| Body | `{ "profile_id": uuid }` |
| Response | `200` `Assignment` |
| Errors | `400` `401` `403` `404` |

#### DELETE /api/profiles/assignments/{scope}

Takes the profile off a scope, so the wider scope's applies there again; `global` is
refused with `409`.

| Field | Value |
|---|---|
| Auth | as `PUT` |
| Path | `scope` `ScopeKey` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `400` `401` `403` `409` |

#### GET /api/profiles/effective

What the profiles in force add up to for a link seen in `channel` of `guild` from
`user`; the whole server's alone without a guild.

| Field | Value |
|---|---|
| Auth | any account |
| Path | — |
| Query | `guild` `snowflake` optional · `channel` `snowflake` optional · `user` `snowflake` optional |
| Body | — |
| Response | `200` `EffectiveView` |
| Errors | `400` `401` |

### Front ends

Front ends are public sites over the server's media, at `/f/<slug>`. Admins manage them
here; what a front end serves to its visitors is under `/api/f/<slug>` below, without an
admin session. See `Frontend`, `FrontendInput`, `FrontInfo` and `FrontJob`.

#### GET /api/frontends

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `Frontend[]` |
| Errors | `401` `403` |

#### POST /api/frontends

Adds a front end. Turning its links on needs `web.public_url`.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Body | `FrontendInput` |
| Response | `201` `Frontend` |
| Errors | `400` `401` `403` `409` |

#### GET /api/frontends/{id}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `Frontend` |
| Errors | `401` `403` `404` |

#### PUT /api/frontends/{id}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `FrontendInput` |
| Response | `200` `Frontend` |
| Errors | `400` `401` `403` `404` `409` |

#### DELETE /api/frontends/{id}

Removes a front end with its accounts and sessions.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

#### PUT /api/frontends/{id}/secret

Stores the shared secret, hashed; `null` removes it. It is never read back.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `{ "secret": string \| null }` |
| Response | `200` `Frontend` |
| Errors | `400` `401` `403` `404` |

#### GET /api/frontends/{id}/users

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `FrontendUser[]` |
| Errors | `401` `403` `404` |

#### POST /api/frontends/{id}/users

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Body | `{ "username": string, "password": string }` |
| Response | `201` `FrontendUser` |
| Errors | `400` `401` `403` `404` `409` |

#### PUT /api/frontends/{id}/users/{user}/password

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `user` `uuid` |
| Body | `{ "password": string }` |
| Response | `200` `FrontendUser` |
| Errors | `400` `401` `403` `404` |

#### DELETE /api/frontends/{id}/users/{user}

Removes an account and its sessions.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `user` `uuid` |
| Response | `204` |
| Errors | `400` `401` `403` `404` |

#### GET /api/frontends/{id}/sessions

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `200` `ViewerSession[]` |
| Errors | `401` `403` `404` |

#### DELETE /api/frontends/{id}/sessions

Ends every viewer session of the front end.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

#### DELETE /api/frontends/{id}/sessions/{session}

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` · `session` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

### Front end visitors

These routes serve a front end's visitors and take no admin session. A viewer's session
is the cookie `dcf_<slug>`. Routes that list or hand out media answer `401` when the
front end asks for a login and the visitor has none, and `404` for a slug that is not an
enabled front end.

#### GET /api/f/{slug}

Who the front end is and how to get in.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Response | `200` `FrontInfo` |
| Errors | `404` |

#### POST /api/f/{slug}/login

Logs in with the shared secret, or with one of the front end's accounts, and sets the
session cookie. Wrong ones count against the address.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Body | `{ "secret": string }` or `{ "username": string, "password": string }` |
| Response | `200` `FrontInfo` |
| Errors | `400` `401` `404` `429` |

#### POST /api/f/{slug}/logout

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Response | `204` |
| Errors | `404` |

#### GET /api/f/{slug}/auth/{provider}/start

Sends the browser to a login provider the front end names; it comes back to `/f/<slug>`
logged in, or to `/f/<slug>/login?error=<reason>` with `reason` one of `state`, `denied`,
`provider`, `exchange`, `identity`, `frontend`, `not_listed`, `not_member`, `guilds`.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` · `provider` |
| Response | `303` to the provider |
| Errors | `400` `404` `429` |

#### GET /api/f/{slug}/jobs

The media the front end shows, newest first.

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` |
| Query | `q` · `media` `MediaKind` · `resolver` · `before` `timestamp` · `limit` (at most 48) |
| Response | `200` `FrontPage` |
| Errors | `401` `404` |

#### GET /api/f/{slug}/jobs/{id}

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` · `id` `uuid` |
| Response | `200` `FrontJob` |
| Errors | `401` `404` |

#### GET /api/f/{slug}/jobs/{id}/media

The output itself, inline, with byte ranges. A signed token `t`, as `FrontJob.media_url`
carries it, opens it without a session until the front end's signed links run out.

| Field | Value |
|---|---|
| Auth | viewer, unless open, or a valid `t` |
| Path | `slug` · `id` `uuid` |
| Query | `t` optional |
| Response | `200` or `206` the file |
| Errors | `401` `404` `416` |

#### GET /api/f/{slug}/jobs/{id}/download

The output as an attachment, when the front end allows downloads.

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` · `id` `uuid` |
| Response | `200` or `206` the file |
| Errors | `401` `403` `404` `416` |

#### GET /f/{slug}/j/{id}

Not under `/api`: the web app's page for one piece of media, with Open Graph and Twitter
card metadata in its head (`og:video` and a direct, signed media link for a video,
`og:audio` for audio, `og:image` for a picture) so link unfurlers play it inline. Any
other `/f/...` path is the web app's shell.

### Account guilds

#### GET /api/discord/guilds

Lists the Discord guilds of the requesting account as last fetched.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Guild[]` |
| Errors | `401` |

#### POST /api/discord/guilds/refresh

Fetches the requesting account's Discord guilds again and stores them.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Guild[]` |
| Errors | `401` `404` `502` |

### Jobs

#### GET /api/jobs

Lists jobs newest first with filters and paging.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | `JobQuery` |
| Body | — |
| Response | `200` `JobPage` |
| Errors | `400` `401` |

#### POST /api/jobs

Queues a link submitted from the web app as a local job.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | — |
| Query | — |
| Body | `SubmitRequest` |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `503` |

#### POST /api/jobs/bulk

Retries or cancels or deletes several jobs and reports each.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | — |
| Query | — |
| Body | `BulkRequest` |
| Response | `200` `BulkResponse` |
| Errors | `400` `401` `403` |

#### GET /api/jobs/stats

Returns the job counts and the workers' load and each resolver's record.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `JobStats` |
| Errors | `401` |

#### GET /api/jobs/events

Streams the job stats and every job event as server-sent events.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` |
| Errors | `401` |

#### GET /api/events

Streams everything the web app watches live on one connection: the job stats and job events of `GET /api/jobs/events` and the bot statuses of `GET /api/discord/bots/events`. The web app holds one such connection per browser, shared by its tabs, so a tab's other requests never wait for a connection.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

#### GET /api/jobs/{id}

Returns one job with its request and stage log and artifacts.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `Job` |
| Errors | `401` `404` |

#### DELETE /api/jobs/{id}

Removes a finished job's record and its cached files.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### POST /api/jobs/{id}/retry

Queues a fresh job with the same request as a finished one.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `404` `409` `503` |

#### POST /api/jobs/{id}/cancel

Stops a queued or running job.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### GET /api/jobs/{id}/download

Streams a job's output or source or subtitle file from the cache or the archive.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | `artifact` `Artifact` default `output` · `index` `integer` default `0` · `inline` `bool` default `false` |
| Body | — |
| Response | `200` file · `206` file with `Range` |
| Errors | `401` `404` `409` `416` |

#### GET /api/jobs/{id}/children

Lists the jobs a playlist job expanded into oldest first.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | — |
| Body | — |
| Response | `200` `JobSummary[]` |
| Errors | `401` `404` |

### Audit log

#### GET /api/audit

Lists audit log entries newest first with filters and paging.

| Field | Value |
|---|---|
| Auth | `view_audit_log` |
| Path | — |
| Query | `AuditQuery` |
| Body | — |
| Response | `200` `Page` |
| Errors | `400` `401` `403` |

### Platforms

#### GET /api/platforms

Lists every platform with what its resolver covers and what its fixtures last found.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `PlatformCoverage[]` |
| Errors | `401` |

#### GET /api/platforms/{id}

Returns one platform with what its fixtures last found.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `string` |
| Query | — |
| Body | — |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `404` |

#### POST /api/platforms/check

Starts a run of the fixtures of every platform not already running them.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | — |
| Query | — |
| Body | — |
| Response | `202` `CheckStarted` |
| Errors | `400` `401` `403` `409` |

#### POST /api/platforms/{id}/check

Starts a run of one platform's fixtures.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` |
| Query | — |
| Body | — |
| Response | `202` `PlatformCoverage` |
| Errors | `400` `401` `403` `404` `409` |

### Platform sessions

#### PUT /api/platforms/{id}/cookies

Replaces a platform's cookies and asks the platform what session they make.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | — |
| Body | `CookiesImport` |
| Response | `200` `SessionOutcome` |
| Errors | `400` `401` `403` `404` |

#### DELETE /api/platforms/{id}/cookies

Removes a platform's cookies.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | — |
| Body | — |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` |

#### POST /api/platforms/{id}/session/check

Asks the platform what its stored cookies are worth now.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | — |
| Body | — |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` `502` |

### Health and metrics

#### GET /api/health

Checks every part the server runs on and sums them up.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Health` |
| Errors | `401` |

#### GET /api/metrics

Measures the process and the machine and the jobs and the HTTP client and the stores.

| Field | Value |
|---|---|
| Auth | any |
| Path | — |
| Query | — |
| Body | — |
| Response | `200` `Metrics` |
| Errors | `401` `500` |

### Server log

#### GET /api/logs

Lists the newest kept log lines that match the filter with paging.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Path | — |
| Query | `LogQuery` |
| Body | — |
| Response | `200` `LogPage` |
| Errors | `400` `401` `403` |

#### GET /api/logs/events

Streams every new log line that matches the filter as server-sent events.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Path | — |
| Query | `LogQuery` without `before` and `limit` |
| Body | — |
| Response | `200` `text/event-stream` · `event: log` · `data: LogLine` · `event: skipped` · `data: Skipped` |
| Errors | `400` `401` `403` |

### Discord slash commands

#### /clip

Queues a video link for download and posts the result in the channel.

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

#### /status

Shows the job counts by status.

| Field | Value |
|---|---|
| Option | — |
| Ephemeral | `DiscoClip <version>: <n> queued, <n> running, <n> done, <n> failed, <n> cancelled` |
| Ephemeral | `Could not read job stats: <error>` |

#### Unknown command

Rejects a command name the bot does not define.

| Field | Value |
|---|---|
| Option | — |
| Ephemeral | `` Unknown command `<name>` `` |

### Request schemas

#### SetupRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | yes |
| `token` | `string` | yes |

#### LoginRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | yes |

#### UserCreateRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | no |
| `role` | `Role` | yes |

#### UserUpdateRequest

| Field | Type | Required |
|---|---|---|
| `role` | `Role` | yes |

#### PasswordRequest

| Field | Type | Required |
|---|---|---|
| `password` | `string` | yes |
| `current_password` | `string` | self only |

#### TokenCreateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `scopes` | `Permission[]` | no · default `[]` |
| `expires_in_days` | `integer` | no |

#### SettingsChange

| Field | Type | Required |
|---|---|---|
| `set` | `object` of dotted key to `json` | no · default `{}` |
| `reset` | `string[]` | no · default `[]` |

#### SettingSetRequest

| Field | Type | Required |
|---|---|---|
| `value` | `json` | yes |

#### SettingsImportRequest

| Field | Type | Required |
|---|---|---|
| `format` | `SettingsFormat` | yes |
| `text` | `string` | yes |

#### ApplicationCreateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | no · default Discord name |
| `bot_token` | `string` | yes |
| `client_secret` | `string` | no |

#### ApplicationUpdateRequest

| Field | Type | Required |
|---|---|---|
| `name` | `string` | no |
| `bot_token` | `string` | no |
| `client_secret` | `string \| null` | no |
| `login` | `bool` | no |

#### CommandScope

| Field | Type | Required |
|---|---|---|
| `mode` | `CommandMode` | yes |
| `guilds` | `snowflake[]` | no · default `[]` |

#### RuleInput

| Field | Type | Required |
|---|---|---|
| `channel_id` | `snowflake` | yes |
| `post_to` | `snowflake \| null` | no |
| `allow_hosts` | `string[]` | no · default `[]` |
| `allow_users` | `snowflake[]` | no · default `[]` |
| `allow_roles` | `snowflake[]` | no · default `[]` |
| `max_source_bytes` | `integer \| null` | no |
| `max_duration_secs` | `integer \| null` | no |
| `max_height` | `integer \| null` | no |
| `enabled` | `bool` | no · default `true` |

#### ProfileInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `description` | `string` | no |
| `platforms` | `PlatformToggles` | no |

#### PlatformToggles

| Field | Type | Required |
|---|---|---|
| `default` | `PlatformDefault` — ignored while `presets` is non-empty | no, `inherit` |
| `presets` | `string[]` — preset ids; when any, the whitelist: platforms in any chosen preset are on, all others off | no |
| `overrides` | `object` of platform id → `bool` — win over presets and the default | no |

#### FrontendInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `slug` | `string` — lower-case letters, digits and dashes | yes |
| `description` | `string` | no |
| `enabled` | `bool` | no, `true` |
| `profile_id` | `uuid` — the platforms shown | no, the built-in profile |
| `scope` | `ContentScope` | no, everything |
| `access` | `Access` | no, closed with no way in |
| `downloads` | `bool` | no, `true` |
| `links` | `LinkPolicy` | no, off |

#### ContentScope

| Field | Type | Required |
|---|---|---|
| `guilds` | `snowflake[]` | no |
| `channels` | `snowflake[]` | no |

Empty on both sides means every job; otherwise jobs seen in any listed guild or channel.

#### Access

| Field | Type | Required |
|---|---|---|
| `open` | `bool` — everyone gets in | no |
| `secret_kind` | `"pin" \| "password" \| "token" \| null` — how the shared secret is asked for | no |
| `accounts` | `bool` — the front end's own accounts may log in | no |
| `providers` | `string[]` — login provider ids | no |
| `discord_members` | `bool` — a Discord login must belong to every guild in the scope | no |
| `discord_users` | `snowflake[]` — a Discord login must be one of these | no |

#### LinkPolicy

| Field | Type | Required |
|---|---|---|
| `enabled` | `bool` | no, `false` |
| `min_height` | `integer` — pixels | no, `720` |
| `min_bitrate` | `integer` — bits per second | no, `1500000` |
| `max_bytes` | `integer` — bound of the output made for the page | no, 2 GiB |
| `signed_link_days` | `integer` | no, `30` |

#### SubmitRequest

| Field | Type | Required |
|---|---|---|
| `url` | `url` | yes |
| `limits` | `RequestLimits` | no |
| `options` | `RequestOptions` | no |

#### BulkRequest

| Field | Type | Required |
|---|---|---|
| `action` | `BulkAction` | yes |
| `ids` | `uuid[]` | yes · 1 to 500 |

#### JobQuery

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

#### CookiesImport

| Field | Type | Required |
|---|---|---|
| `format` | `CookieFormat` | yes |
| `text` | `string` | yes |
| `domain` | `string` | `header` only · default the platform's first host |

#### LogQuery

| Field | Type | Required |
|---|---|---|
| `level` | `LogLevel` | no |
| `target` | `string` | no |
| `q` | `string` | no |
| `before` | `integer` | no |
| `limit` | `integer` | no · default `200` · max `1000` |

#### AuditQuery

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

### Response schemas

#### SetupStatus

| Field | Type |
|---|---|
| `needed` | `bool` |

#### WhoAmI

| Field | Type |
|---|---|
| `user` | `User` |
| `csrf_token` | `string \| null` |
| `session` | `SessionView \| null` |
| `token` | `ApiToken \| null` |

#### SessionView

| Field | Type |
|---|---|
| `id` | `uuid` |
| `created_at` | `timestamp` |
| `last_seen_at` | `timestamp` |
| `expires_at` | `timestamp` |
| `user_agent` | `string \| null` |
| `ip` | `ip \| null` |
| `current` | `bool` |

#### AccountSessionView

| Field | Type |
|---|---|
| `user_id` | `uuid` |
| `username` | `string` |
| `...SessionView` | `SessionView` |

#### Revoked

| Field | Type |
|---|---|
| `revoked` | `integer` |

#### RoleView

| Field | Type |
|---|---|
| `role` | `Role` |
| `description` | `string` |
| `permissions` | `Permission[]` |
| `accounts` | `User[]` |

#### User

| Field | Type |
|---|---|
| `id` | `uuid` |
| `username` | `string` |
| `role` | `Role` |
| `has_password` | `bool` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

#### ApiToken

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

#### AccountTokenView

| Field | Type |
|---|---|
| `username` | `string` |
| `...ApiToken` | `ApiToken` |

#### Minted

| Field | Type |
|---|---|
| `token` | `ApiToken` |
| `secret` | `string` |

#### ProviderInfo

| Field | Type |
|---|---|
| `id` | `string` |
| `name` | `string` |

#### Identity

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

#### Unlinked

| Field | Type |
|---|---|
| `identity` | `Identity` |
| `revoked` | `bool` |

#### CallbackRedirect

| Intent | Outcome | `Location` |
|---|---|---|
| `login` | success | `/` |
| `link` | success | `/account` |
| `login` | failure | `/login?error=<CallbackError>` |
| `link` | failure | `/account?error=<CallbackError>` |

#### SettingsView

| Field | Type |
|---|---|
| `settings` | `object` |
| `defaults` | `object` |
| `entries` | `SettingEntry[]` |
| `secrets` | `string[]` |
| `data_dir` | `string` |
| `provisioning_file` | `string \| null` |

#### SettingEntry

| Field | Type |
|---|---|
| `key` | `string` |
| `source` | `SettingSource` |
| `updated_at` | `timestamp` |

#### ApplicationView

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

#### CommandsState

| Field | Type |
|---|---|
| `mode` | `CommandMode` |
| `guilds` | `snowflake[]` |
| `registered_at` | `timestamp \| null` |
| `error` | `string \| null` |

#### CommandsView

| Field | Type |
|---|---|
| `mode` | `CommandMode` |
| `guilds` | `snowflake[]` |
| `registered_at` | `timestamp \| null` |
| `error` | `string \| null` |
| `commands` | `CommandSummary[]` |

#### CommandSummary

| Field | Type |
|---|---|
| `name` | `string` |
| `description` | `string` |

#### InstallLink

| Field | Type |
|---|---|
| `url` | `url` |
| `scopes` | `string[]` |
| `permissions` | `string[]` |

#### BotStatus

| Field | Type | Present for `state` |
|---|---|---|
| `state` | `BotState` | all |
| `since` | `timestamp` | all |
| `user` | `string` | `connected` |
| `error` | `string` | `retrying` `failed` |
| `attempt` | `integer` | `retrying` |
| `next_attempt_at` | `timestamp` | `retrying` |

#### BotEvent

| Field | Type |
|---|---|
| `application` | `uuid` |
| `...BotStatus` | `BotStatus` |
| `removed` | `true`, only when the application was removed: the last event about its bot |

#### BotGuild

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

#### GuildChannel

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `kind` | `ChannelKind` |
| `parent_id` | `snowflake \| null` |
| `position` | `integer` |
| `rule` | `uuid \| null` |

#### GuildRole

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `color` | `integer` |
| `position` | `integer` |
| `managed` | `bool` |

#### GuildMember

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `username` | `string` |
| `display_name` | `string \| null` |
| `nick` | `string \| null` |
| `avatar` | `string \| null` |
| `bot` | `bool` |

#### Rule

| Field | Type |
|---|---|
| `id` | `uuid` |
| `application_id` | `uuid` |
| `guild_id` | `snowflake` |
| `...RuleInput` | `RuleInput` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

#### Profile

| Field | Type |
|---|---|
| `id` | `uuid` |
| `...ProfileInput` | `ProfileInput` |
| `builtin` | `bool` — ships with the server, cannot be removed |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

#### Preset

| Field | Type |
|---|---|
| `id` | `string` — `basic`, `sfw`, `nsfw`, `news`, `social`, `video`, `music`, `podcasts`, `live`, `files`, `images` or `players` |
| `label` | `string` |
| `description` | `string` |
| `platforms` | `string[]` — resolver ids in it |

#### Scope

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"global" \| "guild" \| "channel" \| "user"` | all |
| `guild_id` | `snowflake` | `guild` `channel` `user` |
| `channel_id` | `snowflake` | `channel` |
| `user_id` | `snowflake` | `user` |

A `ScopeKey` in a path is the same scope as one string: `global`, `guild:<guild>`,
`channel:<guild>:<channel>` or `user:<guild>:<user>`.

#### Assignment

| Field | Type |
|---|---|
| `scope` | `Scope` |
| `profile_id` | `uuid` |
| `updated_at` | `timestamp` |

#### EffectiveProfile

| Field | Type |
|---|---|
| `platforms` | `object` of platform id → `bool` — every platform, on or off |
| `applied` | `Assignment[]` — the assignments applied, widest first |

#### EffectiveView

| Field | Type |
|---|---|
| `...EffectiveProfile` | `EffectiveProfile` |
| `disabled` | `string[]` — the platform ids turned off |

#### Frontend

| Field | Type |
|---|---|
| `id` | `uuid` |
| `...FrontendInput` | `FrontendInput` |
| `has_secret` | `bool` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

#### FrontendUser

| Field | Type |
|---|---|
| `id` | `uuid` |
| `frontend_id` | `uuid` |
| `username` | `string` |
| `created_at` | `timestamp` |

#### ViewerSession

| Field | Type |
|---|---|
| `id` | `uuid` |
| `frontend_id` | `uuid` |
| `subject` | `string` — `secret`, `account:<username>` or `provider:<id>:<subject>` |
| `display` | `string` |
| `created_at` | `timestamp` |
| `last_seen_at` | `timestamp` |
| `expires_at` | `timestamp` |
| `ip` | `string \| null` |
| `user_agent` | `string \| null` |

#### FrontInfo

| Field | Type |
|---|---|
| `slug` | `string` |
| `name` | `string` |
| `description` | `string` |
| `downloads` | `bool` |
| `access` | `{ open, secret: "pin" \| "password" \| "token" \| null, accounts, providers: [{ id, name }], discord_members }` |
| `platforms` | `string[]` — resolver ids shown |
| `viewer` | `{ frontend_id, subject, display } \| null` |

#### FrontPage

| Field | Type |
|---|---|
| `jobs` | `FrontJob[]` |
| `next` | `timestamp \| null` — the `before` of the next page |

#### FrontJob

| Field | Type |
|---|---|
| `id` | `uuid` |
| `title` | `string \| null` |
| `media` | `MediaKind` |
| `resolver` | `string` |
| `uploader` | `string \| null` |
| `webpage_url` | `url \| null` |
| `thumbnail` | `url \| null` |
| `duration_secs` | `number \| null` |
| `live` | `bool` — a recorded stream |
| `size` | `integer` |
| `width` | `integer \| null` |
| `height` | `integer \| null` |
| `content_type` | `string` |
| `published_at` | `timestamp` |
| `media_url` | `string` — plays or shows the media, with its signed token |
| `download_url` | `string \| null` |

#### Guild

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `icon` | `string \| null` |
| `owner` | `bool` |
| `permissions` | `string` |
| `manageable` | `bool` |
| `fetched_at` | `timestamp` |

#### Submitted

| Field | Type |
|---|---|
| `id` | `uuid` |

#### BulkResponse

| Field | Type |
|---|---|
| `action` | `BulkAction` |
| `results` | `BulkOutcome[]` |
| `succeeded` | `integer` |
| `failed` | `integer` |

#### BulkOutcome

| Field | Type |
|---|---|
| `id` | `uuid` |
| `ok` | `bool` |
| `error` | `string \| null` |
| `job` | `uuid \| null` |

#### JobPage

| Field | Type |
|---|---|
| `jobs` | `JobSummary[]` |
| `total` | `integer` |
| `limit` | `integer` |
| `offset` | `integer` |

#### JobSummary

| Field | Type |
|---|---|
| `id` | `uuid` |
| `url` | `url` |
| `status` | `JobStatus` |
| `source` | `string` |
| `origin` | `Origin` |
| `destination` | `string \| null` |
| `submitted_by` | `string \| null` |
| `parent` | `uuid \| null` |
| `retry_of` | `uuid \| null` |
| `title` | `string \| null` |
| `resolver` | `string \| null` |
| `media` | `MediaKind` — what the probe found the source to be, else what the resolver said, else `video` |
| `uploader` | `string \| null` |
| `webpage_url` | `url \| null` |
| `thumbnail` | `url \| null` |
| `duration_secs` | `number \| null` |
| `live` | `bool` |
| `output_bytes` | `integer \| null` |
| `published_url` | `url \| null` |
| `published_reference` | `string \| null` |
| `children` | `integer` |
| `archived_files` | `integer` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `started_at` | `timestamp \| null` |
| `finished_at` | `timestamp \| null` |

#### JobStatus

| Field | Type | Present for `status` |
|---|---|---|
| `status` | `StatusKind` | all |
| `stage` | `Stage` | `running` `failed` |
| `message` | `string` | `failed` |

#### Origin

| Field | Type |
|---|---|
| `source` | `string` |
| `reference` | `string` |
| `url` | `url \| null` |

#### JobStats

| Field | Type |
|---|---|
| `counts` | `Stats` |
| `last_24h` | `Stats` |
| `utilisation` | `Utilisation` |
| `queue_depth` | `integer` |
| `active` | `uuid[]` |
| `resolvers` | `ResolverStats[]` |
| `at` | `timestamp` |

#### Stats

| Field | Type |
|---|---|
| `queued` | `integer` |
| `running` | `integer` |
| `done` | `integer` |
| `failed` | `integer` |
| `cancelled` | `integer` |

#### Utilisation

| Field | Type |
|---|---|
| `workers` | `integer` |
| `active` | `integer` |
| `waiting` | `integer` |

#### ResolverStats

| Field | Type |
|---|---|
| `resolver` | `string` |
| `done` | `integer` |
| `failed` | `integer` |
| `last_done_at` | `timestamp \| null` |
| `last_failed_at` | `timestamp \| null` |

#### JobEvent

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

#### Progress

| Field | Type |
|---|---|
| `done` | `integer` |
| `total` | `integer \| null` |

#### LogEntry

| Field | Type |
|---|---|
| `at` | `timestamp` |
| `stage` | `Stage \| null` |
| `message` | `string` |

#### JobRequest

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

#### RequestLimits

| Field | Type |
|---|---|
| `max_source_bytes` | `integer \| null` |
| `max_duration_secs` | `integer \| null` |
| `max_height` | `integer \| null` |

#### RequestOptions

| Field | Type |
|---|---|
| `clip` | `ClipRange \| null` |
| `subtitles` | `SubtitleMode` |
| `subtitle_language` | `string \| null` |

#### ClipRange

| Field | Type |
|---|---|
| `start` | `Duration` |
| `end` | `Duration \| null` |

#### Duration

| Field | Type |
|---|---|
| `secs` | `integer` |
| `nanos` | `integer` |

#### Job

| Field | Type |
|---|---|
| `id` | `uuid` |
| `request` | `JobRequest` |
| `status` | `JobStatus` |
| `artifacts` | `Artifacts` |
| `log` | `LogEntry[]` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `started_at` | `timestamp \| null` |
| `finished_at` | `timestamp \| null` |

#### Artifacts

| Field | Type |
|---|---|
| `resolved` | `Resolved \| null` |
| `source` | `LocalFile \| null` |
| `output` | `LocalFile \| null` |
| `delivery` | `"upload" \| "link"` — whether the output was handed over or a front end's page was posted |
| `link_reason` | `string \| null` — why a link was posted rather than the file |
| `published` | `Published \| null` |
| `archived` | `ArchiveEntry \| null` |
| `subtitles` | `LocalSubtitle[]` |
| `children` | `uuid[]` |
| `timings` | `StageTiming[]` |

#### Resolved

| Field | Type |
|---|---|
| `resolver` | `string` |
| `media` | `MediaKind` — what the link is; decides how it is picked, shrunk and shown |
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

#### Variant

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
| `codecs` | `string \| null` |
| `video_only` | `bool` |
| `audio_only` | `bool` |
| `live` | `bool` |
| `drm` | `string \| null` |
| `cipher` | `Cipher \| null` |

#### Cipher

| Field | Type | Present for `scheme` |
|---|---|---|
| `scheme` | `CipherScheme` | all |
| `key` | `integer[16]` | `aes128_ctr` |
| `nonce` | `integer[8]` | `aes128_ctr` |

#### Codec

| Wire form | Meaning |
|---|---|
| `string` | a known codec or container name |
| `{ "other": string }` | a name outside the known set |

#### SubtitleTrack

| Field | Type |
|---|---|
| `url` | `url` |
| `language` | `string` |
| `name` | `string \| null` |
| `format` | `SubtitleFormat` |
| `auto` | `bool` |
| `headers` | `[string, string][]` |

#### LocalFile

| Field | Type |
|---|---|
| `path` | `string` |
| `size` | `integer` |
| `info` | `MediaInfo \| null` |

#### MediaInfo

| Field | Type |
|---|---|
| `container` | `Codec` |
| `kind` | `MediaKind` — a still image has its picture in `video` with no `fps` |
| `duration` | `Duration \| null` |
| `video` | `VideoTrack \| null` |
| `audio` | `AudioTrack \| null` |

#### VideoTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `width` | `integer` |
| `height` | `integer` |
| `fps` | `number \| null` |
| `bitrate` | `integer \| null` |

#### AudioTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `channels` | `integer` |
| `sample_rate` | `integer` |
| `bitrate` | `integer \| null` |

#### LocalSubtitle

| Field | Type |
|---|---|
| `language` | `string` |
| `name` | `string \| null` |
| `path` | `string` |
| `format` | `SubtitleFormat` |

#### Published

| Field | Type |
|---|---|
| `reference` | `string` |
| `url` | `url \| null` |
| `at` | `timestamp` |

#### ArchiveEntry

| Field | Type |
|---|---|
| `files` | `string[]` |
| `bytes` | `integer` |
| `at` | `timestamp` |

#### StageTiming

| Field | Type |
|---|---|
| `stage` | `Stage` |
| `started_at` | `timestamp` |
| `ended_at` | `timestamp \| null` |

#### Page

| Field | Type |
|---|---|
| `entries` | `Entry[]` |
| `next` | `uuid \| null` |

#### Entry

| Field | Type |
|---|---|
| `id` | `uuid` |
| `at` | `timestamp` |
| `actor` | `Actor` |
| `action` | `Action` |
| `target` | `Target` |
| `details` | `AuditDetails` |

#### PlatformCoverage

| Field | Type |
|---|---|
| `id` | `string` |
| `name` | `string` |
| `hosts` | `string[]` |
| `features` | `string[]` |
| `formats` | `string[]` |
| `media` | `MediaKind[]` — every kind its links can resolve to |
| `tags` | `string[]` — what kind of place it is: the preset ids minus `sfw` |
| `session` | `SessionSupport` |
| `cookies` | `integer` |
| `fixtures` | `FixtureResult[]` |
| `last_run_at` | `timestamp \| null` |
| `last_pass_at` | `timestamp \| null` |
| `last_fail_at` | `timestamp \| null` |
| `passed` | `integer` |
| `failed` | `integer` |
| `running` | `bool` |
| `cookies_updated_at` | `timestamp \| null` |
| `session_check` | `SessionCheckResult \| null` |

#### SessionCheckResult

| Field | Type | Present for `state` |
|---|---|---|
| `state` | `SessionState` | all |
| `account` | `string` | `logged_in` |
| `at` | `timestamp` | all |

#### SessionOutcome

| Field | Type |
|---|---|
| `...PlatformCoverage` | `PlatformCoverage` |
| `check_error` | `string \| null` |

#### FixtureResult

| Field | Type |
|---|---|
| `url` | `url` |
| `status` | `FixtureStatus` |
| `run_at` | `timestamp \| null` |
| `last_pass_at` | `timestamp \| null` |
| `error` | `string \| null` |
| `title` | `string \| null` |
| `found` | `Found \| null` — what the link resolved to, when it did |
| `duration_ms` | `integer \| null` |

#### Found

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"media" \| "playlist"` | all |
| `media` | `MediaKind` | `media` |
| `variants` | `integer` — playable variants | `media` |
| `entries` | `integer` | `playlist` |

#### MediaKind

`"video"`, `"audio"`, `"image"` or `"file"`: a moving picture (animated GIFs count),
sound alone, a still picture, or any other file.

#### CheckStarted

| Field | Type |
|---|---|
| `platforms` | `string[]` |

#### Health

| Field | Type |
|---|---|
| `status` | `HealthStatus` |
| `version` | `string` |
| `started_at` | `timestamp` |
| `uptime_secs` | `integer` |
| `at` | `timestamp` |
| `checks` | `HealthCheck[]` |

#### HealthCheck

| Field | Type |
|---|---|
| `name` | `string` |
| `label` | `string` |
| `status` | `HealthStatus` |
| `detail` | `string` |

| `name` | What is checked |
|---|---|
| `database` | SQLite answers a query |
| `engine` | the workers and their load |
| `cache` | the cache directory takes a file |
| `local` | the local publishing directory takes a file |
| `ffmpeg` | ffmpeg runs and reports its version |
| `bots` | no bot has failed or is retrying or lacks a token |
| `fixtures` | no platform has a failing fixture |

#### Metrics

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

#### ProcessMetrics

| Field | Type |
|---|---|
| `pid` | `integer` |
| `rss_bytes` | `integer` |
| `virtual_bytes` | `integer` |
| `cpu_percent` | `number` |
| `run_time_secs` | `integer` |

#### SystemMetrics

| Field | Type |
|---|---|
| `total_memory_bytes` | `integer` |
| `available_memory_bytes` | `integer` |
| `load_average` | `[number, number, number]` |
| `cpus` | `integer` |
| `disks` | `DiskMetrics[]` |

#### DiskMetrics

| Field | Type |
|---|---|
| `mount` | `string` |
| `total_bytes` | `integer` |
| `available_bytes` | `integer` |
| `holds` | `string[]` of `cache` `data` `local` `archive` |

#### HttpMetrics

| Field | Type |
|---|---|
| `requests` | `RequestCount[]` |
| `retries` | `integer` |
| `rate_limit_waits` | `integer` |
| `bytes_received` | `integer` |

#### RequestCount

| Field | Type |
|---|---|
| `host` | `string` |
| `status` | `integer` |
| `count` | `integer` |

#### BotMetrics

| Field | Type |
|---|---|
| `applications` | `integer` |
| `by_state` | `object` of `BotState` to `integer` |

#### CacheMetrics

| Field | Type |
|---|---|
| `dir` | `string` |
| `bytes` | `integer` |
| `jobs` | `integer` |

#### DatabaseMetrics

| Field | Type |
|---|---|
| `path` | `string` |
| `bytes` | `integer` |

#### FixtureMetrics

| Field | Type |
|---|---|
| `platforms` | `integer` |
| `with_fixtures` | `integer` |
| `passing` | `integer` |
| `failing` | `integer` |
| `never` | `integer` |
| `running` | `integer` |

#### LogMetrics

| Field | Type |
|---|---|
| `buffered` | `integer` |
| `capacity` | `integer` |

#### LogPage

| Field | Type |
|---|---|
| `lines` | `LogLine[]` |
| `next` | `integer \| null` |
| `buffered` | `integer` |
| `capacity` | `integer` |
| `oldest_id` | `integer \| null` |

#### LogLine

| Field | Type |
|---|---|
| `id` | `integer` |
| `at` | `timestamp` |
| `level` | `LogLevel` |
| `target` | `string` |
| `message` | `string` |
| `fields` | `object` of `string` to `string` |

#### Skipped

| Field | Type |
|---|---|
| `count` | `integer` |

#### Actor

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `ActorKind` | all |
| `id` | `uuid` | `user` |
| `username` | `string` | `user` |
| `via` | `Via` | `user` |
| `ip` | `ip` | `user` |
| `file` | `string \| null` | `provisioning` |

#### Target

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

#### AuditDetails

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
| `profile.update` | `previous` | `ProfileInput` |
| `profile.delete` | `assignments_removed` | `integer` |
| `profile.assign` `profile.unassign` | `scope` | `Scope` |
| `profile.assign` | `previous_profile_id` | `uuid \| null` |
| `frontend.create` `frontend.update` `frontend.delete` | `frontend` | `FrontendInput` |
| `frontend.update` | `previous` | `FrontendInput` |
| `frontend.user.create` `frontend.user.password` `frontend.user.delete` | `username` | `string` |
| `frontend.sessions.revoke` | `sessions` | `integer` |
| `frontend.sessions.revoke` | `session_id` | `uuid \| null` |

### Enumerations

#### Role

| Value |
|---|
| `admin` |
| `operator` |
| `viewer` |

#### Permission

| Value |
|---|
| `manage_users` |
| `manage_applications` |
| `manage_watch_rules` |
| `manage_bots` |
| `view_audit_log` |
| `manage_jobs` |
| `manage_settings` |
| `view_logs` |

#### Intent

| Value |
|---|
| `login` |
| `link` |

#### SettingSource

| Value |
|---|
| `provisioning` |
| `app` |

#### SettingsFormat

| Value |
|---|
| `toml` |
| `yaml` |
| `json` |

#### StatusKind

| Value |
|---|
| `queued` |
| `running` |
| `done` |
| `failed` |
| `cancelled` |

#### Stage

| Value |
|---|
| `resolve` |
| `download` |
| `transcode` |
| `publish` |
| `archive` |

#### BulkAction

| Value |
|---|
| `retry` |
| `cancel` |
| `delete` |

#### Artifact

| Value |
|---|
| `output` |
| `source` |
| `subtitle` |

#### JobOrder

| Value |
|---|
| `newest` |
| `oldest` |

#### JobEventKind

| Value |
|---|
| `submitted` |
| `status` |
| `progress` |
| `log` |
| `children` |
| `deleted` |

#### SubtitleMode

| Value |
|---|
| `keep` |
| `burn` |
| `skip` |

#### SubtitleFormat

| Value |
|---|
| `vtt` |
| `srt` |
| `ttml` |
| `ass` |
| `json3` |
| `hls_vtt` |

#### VariantKind

| Value |
|---|
| `file` |
| `hls` |
| `dash` |
| `ism` |
| `rtmp` |
| `rtsp` |
| `whep` |
| `browser` |

#### CipherScheme

| Value |
|---|
| `aes128_ctr` |

#### CallbackError

| Value |
|---|
| `state` |
| `denied` |
| `provider` |
| `identity` |
| `exchange` |
| `session` |
| `already_linked` |
| `provider_linked` |
| `unknown_identity` |

#### CommandMode

| Value |
|---|
| `off` |
| `global` |
| `guilds` |

#### ChannelKind

| Value |
|---|
| `text` |
| `announcement` |
| `voice` |
| `stage` |
| `category` |
| `forum` |
| `media` |
| `thread` |
| `other` |

#### SessionSupport

| Value |
|---|
| `none` |
| `optional` |
| `required` |

#### FixtureStatus

| Value |
|---|
| `pass` |
| `fail` |
| `never` |

#### SessionState

| Value |
|---|
| `unsupported` |
| `logged_out` |
| `logged_in` |

#### CookieFormat

| Value |
|---|
| `netscape` |
| `header` |

#### HealthStatus

| Value |
|---|
| `ok` |
| `warn` |
| `fail` |

#### LogLevel

| Value |
|---|
| `trace` |
| `debug` |
| `info` |
| `warn` |
| `error` |

#### BotState

| Value |
|---|
| `disabled` |
| `stopped` |
| `starting` |
| `connected` |
| `retrying` |
| `failed` |

#### ActorKind

| Value |
|---|
| `user` |
| `provisioning` |

#### Via

| Value |
|---|
| `session` |
| `token` |

#### PlatformDefault

| Value | Meaning |
|---|---|
| `inherit` | Platforms the profile does not name stay as the wider scope has them |
| `enabled` | Platforms the profile does not name are on |
| `disabled` | Platforms the profile does not name are off |

#### TargetKind

| Value |
|---|
| `setting` |
| `application` |
| `rule` |
| `platform` |
| `profile` |
| `frontend` |

#### Action

| Value |
|---|
| `settings.set` |
| `settings.reset` |
| `settings.import` |
| `settings.provision` |
| `application.create` |
| `application.update` |
| `application.delete` |
| `application.commands.set` |
| `application.commands.register` |
| `bot.start` |
| `bot.stop` |
| `bot.restart` |
| `rule.create` |
| `rule.update` |
| `rule.delete` |
| `session.import` |
| `session.clear` |
| `profile.create` |
| `profile.update` |
| `profile.delete` |
| `profile.assign` |
| `profile.unassign` |
| `frontend.create` |
| `frontend.update` |
| `frontend.delete` |
| `frontend.secret.set` |
| `frontend.secret.clear` |
| `frontend.user.create` |
| `frontend.user.password` |
| `frontend.user.delete` |
| `frontend.sessions.revoke` |

#### Install scopes

| Value |
|---|
| `bot` |
| `applications.commands` |

#### Install permissions

| Value |
|---|
| `VIEW_CHANNEL` |
| `SEND_MESSAGES` |
| `SEND_MESSAGES_IN_THREADS` |
| `EMBED_LINKS` |
| `ATTACH_FILES` |
| `READ_MESSAGE_HISTORY` |
