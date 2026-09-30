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
| Wrong recovery key per address | 5 | 15 minutes | `429` |
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
| `429` | `{ "error": "Too many attempts. Try again in <n> seconds." }` + `Retry-After: <n>` |
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

Reports whether an admin account needs to be created.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `SetupStatus` |
| Errors | None |

#### POST /api/setup

Creates the first admin account and starts a session. Refused once any account exists.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | `SetupRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `400` `409` |

#### POST /api/recover

Sets a new password for an account with the recovery key printed on the server console, ends the account's sessions and its login lockout, and starts a session. The key is replaced and printed again.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | `RecoverRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `400` `403` `404` `429` |

#### POST /api/login

Opens a browser session for a username and password.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | `LoginRequest` |
| Response | `200` `WhoAmI` + `Set-Cookie` |
| Errors | `401` `429` |

#### POST /api/logout

Ends the browser session making the request and clears its cookie.

| Field | Value |
|---|---|
| Auth | session |
| Path | None |
| Query | None |
| Body | None |
| Response | `204` + cleared cookie |
| Errors | `401` `403` |

#### GET /api/session

Returns the authenticated account and its session or API token.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `WhoAmI` |
| Errors | `401` |

### Sessions

#### GET /api/sessions

Lists active browser sessions for the current account.

| Field | Value |
|---|---|
| Auth | session |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` |

#### GET /api/sessions/all

Lists active sessions across all accounts, newest first.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `AccountSessionView[]` |
| Errors | `401` `403` |

#### DELETE /api/sessions/others

Ends every session of the requesting account except the current one.

| Field | Value |
|---|---|
| Auth | session |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Revoked` |
| Errors | `401` `403` |

#### DELETE /api/sessions/{id}

Ends one session of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` + cleared cookie when `id` is the current session |
| Errors | `401` `403` `404` |

### Roles

#### GET /api/roles

Lists roles, permissions and assigned accounts.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `RoleView[]` |
| Errors | `401` `403` |

### Users

#### GET /api/users

Lists every account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `User[]` |
| Errors | `401` `403` |

#### POST /api/users

Creates an account with a role and an optional password.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | None |
| Query | None |
| Body | `UserCreateRequest` |
| Response | `201` `User` |
| Errors | `400` `401` `403` `409` |

#### GET /api/users/{id}

Returns one account.

| Field | Value |
|---|---|
| Auth | self or `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `User` |
| Errors | `401` `403` `404` |

#### PATCH /api/users/{id}

Changes an account's role.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | `UserUpdateRequest` |
| Response | `200` `User` |
| Errors | `401` `403` `404` `409` |

#### DELETE /api/users/{id}

Deletes an account, its sessions, tokens and provider grants.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### PUT /api/users/{id}/password

Sets an account's password and ends its other sessions.

| Field | Value |
|---|---|
| Auth | self with `current_password` or `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | `PasswordRequest` |
| Response | `204` |
| Errors | `400` `401` `403` `404` `429` |

#### GET /api/users/{id}/sessions

Lists the live browser sessions of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `SessionView[]` |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/sessions

Ends every browser session of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `Revoked` + cleared cookie when `id` is the requesting account |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/sessions/{session}

Ends one browser session of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `session` `uuid` |
| Query | None |
| Body | None |
| Response | `204` + cleared cookie when `session` is the requesting session |
| Errors | `401` `403` `404` |

### API tokens

#### GET /api/tokens

Lists the API tokens of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` |

#### POST /api/tokens

Creates an API token. The response includes the secret once.

| Field | Value |
|---|---|
| Auth | session |
| Path | None |
| Query | None |
| Body | `TokenCreateRequest` |
| Response | `201` `Minted` |
| Errors | `400` `401` `403` |

#### GET /api/tokens/all

Lists active API tokens across all accounts, newest first.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `AccountTokenView[]` |
| Errors | `401` `403` |

#### DELETE /api/tokens/{id}

Revokes one API token of the requesting account.

| Field | Value |
|---|---|
| Auth | session |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` |

#### GET /api/users/{id}/tokens

Lists the API tokens of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `ApiToken[]` |
| Errors | `401` `403` `404` |

#### DELETE /api/users/{id}/tokens/{token}

Revokes one API token of an account.

| Field | Value |
|---|---|
| Auth | `manage_users` |
| Path | `id` `uuid` · `token` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` |

### Login providers

#### GET /api/auth/providers

Lists the login providers the server offers.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `ProviderInfo[]` |
| Errors | None |

#### GET /api/auth/{provider}/start

Redirects the browser to a provider to log in or link an identity.

| Field | Value |
|---|---|
| Auth | none for `intent=login` · any for `intent=link` |
| Path | `provider` `string` |
| Query | `intent` `Intent` default `login` |
| Body | None |
| Response | `303` `Location: <provider authorize url>` |
| Errors | `400` `401` `404` `429` `502` |

#### GET /api/auth/{provider}/callback

Completes a provider flow and redirects the browser into the app.

| Field | Value |
|---|---|
| Auth | none for `login` · session of the linking account for `link` |
| Path | `provider` `string` |
| Query | `code` `string` · `state` `string` · `error` `string` |
| Body | None |
| Response | `303` `Location` per `CallbackRedirect` + `Set-Cookie` after a login |
| Errors | None |

#### GET /api/auth/identities

Lists the provider identities linked to the requesting account.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Identity[]` |
| Errors | `401` |

#### DELETE /api/auth/identities/{provider}

Unlinks a provider identity and revokes its grant at the provider.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Query | None |
| Body | None |
| Response | `200` `Unlinked` |
| Errors | `401` `404` `409` |

#### POST /api/auth/identities/{provider}/refresh

Refreshes a linked identity and its provider tokens.

| Field | Value |
|---|---|
| Auth | any |
| Path | `provider` `string` |
| Query | None |
| Body | None |
| Response | `200` `Identity` |
| Errors | `401` `404` `409` `502` |

### Settings

#### GET /api/settings

Returns settings, defaults and saved value sources.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `SettingsView` |
| Errors | `401` `403` |

#### PATCH /api/settings

Sets and resets multiple settings in one operation. Changes apply immediately.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | `SettingsChange` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### PUT /api/settings/{key}

Saves and applies a value at a dotted key. A secret sent as `null`, alone or inside a section, keeps the value already stored for it, which is how the withheld secrets of `SettingsView` round-trip.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Query | None |
| Body | `SettingSetRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### DELETE /api/settings/{key}

Deletes a saved key and its children, restoring defaults.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `key` `string` |
| Query | None |
| Body | None |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### POST /api/settings/import

Imports and applies config values as app settings.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | `SettingsImportRequest` |
| Response | `200` `SettingsView` |
| Errors | `400` `401` `403` |

#### GET /api/settings/export

Exports saved settings as a config file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | `format` `SettingsFormat` default `toml` |
| Body | None |
| Response | `200` `application/toml` `application/yaml` `application/json` file |
| Errors | `400` `401` `403` |

### Discord applications

#### GET /api/discord/applications

Lists every Discord application with its bot state and install link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `ApplicationView[]` |
| Errors | `401` `403` |

#### POST /api/discord/applications

Adds a Discord application by its bot token and launches its bot.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | None |
| Query | None |
| Body | `ApplicationCreateRequest` |
| Response | `201` `ApplicationView` |
| Errors | `400` `401` `403` `409` `502` |

#### GET /api/discord/applications/{id}

Returns one Discord application with its bot state and install link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` |

#### PATCH /api/discord/applications/{id}

Updates an application name, credentials or login setting.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | `ApplicationUpdateRequest` |
| Response | `200` `ApplicationView` |
| Errors | `400` `401` `403` `404` `502` |

#### DELETE /api/discord/applications/{id}

Stops the bot and deletes its application, rules and server records.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/install

Returns a bot installation link.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | `guild` `snowflake` |
| Body | None |
| Response | `200` `InstallLink` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/guilds

Lists current and previous Discord servers for the bot.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `BotGuild[]` |
| Errors | `401` `403` `404` |

#### GET /api/discord/applications/{id}/guilds/{guild}/channels

Lists channels visible to the bot and their watch rules.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | None |
| Body | None |
| Response | `200` `GuildChannel[]` |
| Errors | `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/roles

Lists server roles visible to the bot.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | None |
| Body | None |
| Response | `200` `GuildRole[]` |
| Errors | `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/members

Searches Discord server members by name.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | `q` `string` · `limit` `integer` default `20` max `100` |
| Body | None |
| Response | `200` `GuildMember[]` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### GET /api/discord/applications/{id}/guilds/{guild}/members/{user}

Returns a Discord server member by ID.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` · `user` `snowflake` |
| Query | None |
| Body | None |
| Response | `200` `GuildMember` |
| Errors | `400` `401` `403` `404` `409` `502` |

### Slash command registration

#### GET /api/discord/applications/{id}/commands

Returns the registered commands, scope and latest registration result.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` |

#### PUT /api/discord/applications/{id}/commands

Updates command registration scope and registers the commands.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | `CommandScope` |
| Response | `200` `CommandsView` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### POST /api/discord/applications/{id}/commands/register

Registers commands again using the saved scope.

| Field | Value |
|---|---|
| Auth | `manage_applications` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `CommandsView` |
| Errors | `401` `403` `404` `409` `502` |

### Bots

#### POST /api/discord/applications/{id}/bot/start

Enables and starts the bot.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### POST /api/discord/applications/{id}/bot/stop

Stops the bot until explicitly started.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### POST /api/discord/applications/{id}/bot/restart

Enables and restarts the bot.

| Field | Value |
|---|---|
| Auth | `manage_bots` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `ApplicationView` |
| Errors | `401` `403` `404` `409` |

#### GET /api/discord/bots/events

Streams initial bot states and subsequent changes as server-sent events.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `text/event-stream` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

### Watch rules

A rule watches one channel, or with `channel_id` `null`, every channel of the server. A
channel's own rule takes the server's place there: enabled, the channel is watched with the
rule's own settings; disabled, the channel is left alone.

#### GET /api/discord/applications/{id}/guilds/{guild}/rules

Lists watch rules for an application and Discord server.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | None |
| Body | None |
| Response | `200` `Rule[]` |
| Errors | `401` `403` `404` |

#### POST /api/discord/applications/{id}/guilds/{guild}/rules

Creates a watch rule. Returns `409` when the channel, or the server as a whole, already has one.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `id` `uuid` · `guild` `snowflake` |
| Query | None |
| Body | `RuleInput` |
| Response | `201` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### GET /api/discord/rules

Lists all watch rules.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Rule[]` |
| Errors | `401` `403` |

#### GET /api/discord/rules/{id}

Returns one watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `Rule` |
| Errors | `401` `403` `404` |

#### PUT /api/discord/rules/{id}

Updates a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | None |
| Body | `RuleInput` |
| Response | `200` `Rule` |
| Errors | `400` `401` `403` `404` `409` `502` |

#### DELETE /api/discord/rules/{id}

Removes a watch rule.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing the rule's guild |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` |

### Profiles

Profiles control platform access and media limits. Assignments apply from global to server, channel and member. See `Profile`, `Scope` and `EffectiveProfile`.

#### GET /api/profiles

Lists every profile.

| Field | Value |
|---|---|
| Auth | any account |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Profile[]` |
| Errors | `401` |

#### POST /api/profiles

Adds a profile.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | `ProfileInput` |
| Response | `201` `Profile` |
| Errors | `400` `401` `403` `409` |

#### GET /api/profiles/{id}

Returns one profile.

| Field | Value |
|---|---|
| Auth | any account |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `Profile` |
| Errors | `401` `404` |

#### PUT /api/profiles/{id}

Updates a profile.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Query | None |
| Body | `ProfileInput` |
| Response | `200` `Profile` |
| Errors | `400` `401` `403` `404` `409` |

#### DELETE /api/profiles/{id}

Deletes a profile and its assignments. Returns `409` for the built-in profile or current global default.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### GET /api/profiles/presets

Lists category presets and their platform IDs.

| Field | Value |
|---|---|
| Auth | any account |
| Response | `200` `Preset[]` |
| Errors | `401` |

#### GET /api/profiles/assignments

Lists profile assignments. With `guild`, returns global and matching server assignments. Without it, returns all assignments.

| Field | Value |
|---|---|
| Auth | with `guild`: `manage_watch_rules` or session managing `guild`. Without: `manage_watch_rules` |
| Path | None |
| Query | `guild` `snowflake` optional |
| Body | None |
| Response | `200` `Assignment[]` |
| Errors | `401` `403` |

#### PUT /api/profiles/assignments/{scope}

Assigns a profile to a scope, replacing the previous assignment.

| Field | Value |
|---|---|
| Auth | `global`: `manage_settings`. A guild's scopes: `manage_watch_rules` or session managing the guild |
| Path | `scope` `ScopeKey` |
| Query | None |
| Body | `{ "profile_id": uuid }` |
| Response | `200` `Assignment` |
| Errors | `400` `401` `403` `404` |

#### DELETE /api/profiles/assignments/{scope}

Removes an assignment to restore inheritance. Returns `409` for the global scope.

| Field | Value |
|---|---|
| Auth | as `PUT` |
| Path | `scope` `ScopeKey` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `400` `401` `403` `409` |

#### GET /api/profiles/effective

Returns effective settings for `channel`, `guild` and `user`. Without `guild`, returns global settings.

| Field | Value |
|---|---|
| Auth | any account |
| Path | None |
| Query | `guild` `snowflake` optional · `channel` `snowflake` optional · `user` `snowflake` optional |
| Body | None |
| Response | `200` `EffectiveView` |
| Errors | `400` `401` |

### Content views

Content views share completed media at `/f/<slug>`. The API calls a view a frontend. Admin routes manage views. Viewer routes use `/api/f/<slug>` and separate sessions. See `Frontend`, `FrontendInput`, `FrontInfo` and `FrontJob`.

#### GET /api/frontends

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Response | `200` `Frontend[]` |
| Errors | `401` `403` |

#### POST /api/frontends

Creates a content view. Discord links require `web.public_url`.

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

Deletes a content view, its accounts and sessions.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `uuid` |
| Response | `204` |
| Errors | `401` `403` `404` |

#### PUT /api/frontends/{id}/secret

Hashes and saves the shared secret. Pass `null` to remove it. Secrets cannot be retrieved.

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

Ends all viewer sessions of the view.

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

### Content view visitors

Viewer routes use the `dcf_<slug>` session cookie. Media routes return `401` when login is required and `404` for missing or disabled views.

#### GET /api/f/{slug}

Returns the view's details and available login methods.

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` |
| Response | `200` `FrontInfo` |
| Errors | `404` |

#### POST /api/f/{slug}/login

Authenticates a shared secret or view account and sets a session cookie. Failed attempts are rate limited by address.

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

Redirects to a configured login provider. Success returns to `/f/<slug>`. Failure returns to `/f/<slug>/login?error=<reason>`. Reasons: `state`, `denied`, `provider`, `exchange`, `identity`, `frontend`, `not_listed`, `not_member`, `guilds`, `channels` (the view requires membership and no running bot names the server of a listed channel).

| Field | Value |
|---|---|
| Auth | none |
| Path | `slug` · `provider` |
| Response | `303` to the provider |
| Errors | `400` `404` `429` |

#### GET /api/f/{slug}/jobs

Lists the view's media, newest first.

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

Streams the output with byte-range support. The signed `t` token in `FrontJob.media_url` permits access without a session until expiry.

| Field | Value |
|---|---|
| Auth | viewer, unless open, or a valid `t` |
| Path | `slug` · `id` `uuid` |
| Query | `t` optional |
| Response | `200` or `206` the file |
| Errors | `401` `404` `416` |

#### GET /api/f/{slug}/jobs/{id}/download

Downloads the output as an attachment when the view allows downloads.

| Field | Value |
|---|---|
| Auth | viewer, unless open |
| Path | `slug` · `id` `uuid` |
| Response | `200` or `206` the file |
| Errors | `401` `403` `404` `416` |

#### GET /f/{slug}/j/{id}

Returns a media page with Open Graph and Twitter preview metadata. Video, audio and image previews use signed media links. Other `/f/...` paths return the web app shell.

### Account guilds

#### GET /api/discord/guilds

Returns the cached Discord server list for the current account.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Guild[]` |
| Errors | `401` |

#### POST /api/discord/guilds/refresh

Refreshes the current account Discord server list.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Guild[]` |
| Errors | `401` `404` `502` |

#### GET /api/discord/guilds/{guild}/applications

Lists the applications whose bots have been in a Discord server.

| Field | Value |
|---|---|
| Auth | `manage_watch_rules` or session managing `guild` |
| Path | `guild` `snowflake` |
| Query | None |
| Body | None |
| Response | `200` `GuildApplication[]` |
| Errors | `401` `403` |

### Jobs

#### GET /api/jobs

Lists jobs newest first with filters and paging.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | `JobQuery` |
| Body | None |
| Response | `200` `JobPage` |
| Errors | `400` `401` |

#### POST /api/jobs

Queues a link submitted from the web app as a local job.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | None |
| Query | None |
| Body | `SubmitRequest` |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `503` |

#### POST /api/jobs/bulk

Retries, cancels, stops or deletes multiple jobs and returns individual results.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | None |
| Query | None |
| Body | `BulkRequest` |
| Response | `200` `BulkResponse` |
| Errors | `400` `401` `403` |

#### GET /api/jobs/stats

Returns job counts, worker load and resolver results.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `JobStats` |
| Errors | `401` |

#### GET /api/jobs/events

Streams the job stats and every job event as server-sent events.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` |
| Errors | `401` |

#### GET /api/events

Streams job statistics, job events and bot status on one connection. Browser tabs share this stream to avoid exhausting connections.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `text/event-stream` · `event: stats` · `data: JobStats` · `event: job` · `data: JobEvent` · `event: bot` · `data: BotEvent` |
| Errors | `401` |

#### GET /api/jobs/{id}

Returns a job, its request, stage log and artifacts.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `Job` |
| Errors | `401` `404` |

#### DELETE /api/jobs/{id}

Removes a finished job's record and its cached files.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### POST /api/jobs/{id}/retry

Queues a fresh job with the same request as a finished one.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `202` `Submitted` |
| Errors | `400` `401` `403` `404` `409` `503` |

#### POST /api/jobs/{id}/cancel

Stops a queued or running job. A live capture's recording is deleted with the job's directory.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### POST /api/jobs/{id}/stop

Ends a running live capture, keeping what was recorded. The job goes on to make and post its output from the recording as it stands. `409` when the job is not capturing a live stream.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `409` |

#### GET /api/jobs/{id}/download

Streams an output, source, recording or subtitle file from cache or archive. The recording of a capture under way is served as it grows: its length is read for every request, a `Range` is answered with `Content-Range: bytes a-b/*`, and a request for bytes past its end waits up to ten seconds for them.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | `artifact` `Artifact` default `output` · `index` `integer` default `0` · `inline` `bool` default `false` |
| Body | None |
| Response | `200` file · `206` file with `Range` |
| Errors | `401` `404` `409` `416` |

#### GET /api/jobs/{id}/children

Lists playlist child jobs, oldest first.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `JobSummary[]` |
| Errors | `401` `404` |

### Audit log

#### GET /api/audit

Lists audit log entries newest first with filters and paging.

| Field | Value |
|---|---|
| Auth | `view_audit_log` |
| Path | None |
| Query | `AuditQuery` |
| Body | None |
| Response | `200` `Page` |
| Errors | `400` `401` `403` |

### Platforms

#### GET /api/platforms

Lists platform capabilities and latest test results.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `PlatformCoverage[]` |
| Errors | `401` |

#### GET /api/platforms/{id}

Returns platform details and latest test results.

| Field | Value |
|---|---|
| Auth | any |
| Path | `id` `string` |
| Query | None |
| Body | None |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `404` |

#### POST /api/platforms/check

Starts platform tests, skipping those already running.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | None |
| Query | None |
| Body | None |
| Response | `202` `CheckStarted` |
| Errors | `400` `401` `403` `409` |

#### POST /api/platforms/{id}/check

Starts tests for one platform.

| Field | Value |
|---|---|
| Auth | `manage_jobs` |
| Path | `id` `string` |
| Query | None |
| Body | None |
| Response | `202` `PlatformCoverage` |
| Errors | `400` `401` `403` `404` `409` |

### Platform sessions

#### PUT /api/platforms/{id}/cookies

Replaces platform cookies and checks the resulting session.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | None |
| Body | `CookiesImport` |
| Response | `200` `SessionOutcome` |
| Errors | `400` `401` `403` `404` |

#### DELETE /api/platforms/{id}/cookies

Removes a platform's cookies.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | None |
| Body | None |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` |

#### POST /api/platforms/{id}/session/check

Checks the saved platform session.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `id` `string` |
| Query | None |
| Body | None |
| Response | `200` `PlatformCoverage` |
| Errors | `401` `403` `404` `502` |

### Health and metrics

#### GET /api/health

Returns component health checks and overall status.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Health` |
| Errors | `401` |

#### GET /api/metrics

Returns process, host, job, HTTP and storage metrics.

| Field | Value |
|---|---|
| Auth | any |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `Metrics` |
| Errors | `401` `500` |

### Backups and retention

#### GET /api/backups

Lists the database backups on disk with the schedule and the last run.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `BackupsView` |
| Errors | `401` `403` `500` |

#### POST /api/backups

Makes a backup of the database now and removes the ones past the kept count. This works even when the automatic schedule is off.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | None |
| Response | `201` `BackupEntry` |
| Errors | `401` `403` `409` `500` |

#### GET /api/backups/{name}

Downloads one backup file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Query | None |
| Body | None |
| Response | `200` `application/octet-stream` file with `Content-Disposition: attachment` |
| Errors | `401` `403` `404` `416` |

#### DELETE /api/backups/{name}

Removes one backup file.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Query | None |
| Body | None |
| Response | `204` |
| Errors | `401` `403` `404` `500` |

#### POST /api/backups/{name}/restore

Validates a private copy of the selected backup, then stops the services, saves a safety backup, restores the database, and restarts automatically. Server connection settings (`web`) and the backup directory are preserved. Restored sessions are invalidated. If the restored services cannot start, the previous database is recovered automatically.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | `name` `string` |
| Query | None |
| Body | None |
| Response | `202` `RestoreStatus` |
| Errors | `401` `403` `404` `409` `500` |

The response includes an unguessable receipt ID for checking progress after the connection closes. Disconnecting the client does not cancel an accepted restore. Concurrent backup creation, deletion, and restore requests are refused until the restore finishes. The safety copy is not rotated during restoration.

#### GET /api/backups/restore/{id}

Returns progress for a restore receipt. This endpoint works after the restored database invalidates the initiating session. It exposes only the phase and a generic failure message, not database contents. Receipts last for the process lifetime, until another restore replaces them.

| Field | Value |
|---|---|
| Auth | possession of the restore receipt ID |
| Path | `id` `uuid` |
| Query | None |
| Body | None |
| Response | `200` `RestoreStatus` |
| Errors | `400` `404` |

#### GET /api/retention

Returns the retention settings and what the sweeps have done.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `RetentionView` |
| Errors | `401` `403` |

#### POST /api/retention/sweep

Runs a retention sweep now.

| Field | Value |
|---|---|
| Auth | `manage_settings` |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `SweepReport` |
| Errors | `401` `403` |

### Operations

These two live at the root rather than under `/api`.

#### GET /healthz

Answers 200 while the server works and 503 when a part failed or it is stopping.

| Field | Value |
|---|---|
| Auth | none |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `503` `Healthz` |
| Errors | None |

#### GET /metrics

Serves the Prometheus text exposition to scrapers and the web app's Metrics page to browsers.

| Field | Value |
|---|---|
| Auth | any for the exposition |
| Path | None |
| Query | None |
| Body | None |
| Response | `200` `text/plain; version=0.0.4` unless `Accept` includes `text/html` |
| Errors | `401` `500` |

### Server log

#### GET /api/logs

Lists retained log entries with filtering and pagination.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Path | None |
| Query | `LogQuery` |
| Body | None |
| Response | `200` `LogPage` |
| Errors | `400` `401` `403` |

#### GET /api/logs/events

Streams every new log line that matches the filter as server-sent events.

| Field | Value |
|---|---|
| Auth | `view_logs` |
| Path | None |
| Query | `LogQuery` without `before` and `limit` |
| Body | None |
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
| Option | None |
| Ephemeral | `DiscoClip <version>: <n> queued, <n> running, <n> done, <n> failed, <n> cancelled` |
| Ephemeral | `Could not read job stats: <error>` |

#### Unknown command

Rejects a command name the bot does not define.

| Field | Value |
|---|---|
| Option | None |
| Ephemeral | `` Unknown command `<name>` `` |

### Request schemas

#### SetupRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `password` | `string` | yes |

#### RecoverRequest

| Field | Type | Required |
|---|---|---|
| `username` | `string` | yes |
| `key` | `string`: the recovery key from the server console | yes |
| `password` | `string`: the new password | yes |

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
| `channel_id` | `snowflake \| null` | yes · `null` watches every channel of the server |
| `post_to` | `snowflake \| null` | no · the channel the link was posted in when absent |
| `allow_users` | `snowflake[]` | no · default `[]` |
| `allow_roles` | `snowflake[]` | no · default `[]` |
| `enabled` | `bool` | no · default `true` |

#### ProfileInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `description` | `string` | no |
| `platforms` | `PlatformToggles` | no |
| `limits` | `ProfileLimits` | no, none named |
| `audio_language` | `string \| null`: the language of the sound taken when a source offers several, as a language tag such as `en` or `pt-br`. Unset leaves the parent scope's | no |

#### ProfileLimits

| Field | Type | Required |
|---|---|---|
| `max_source_bytes` | `integer \| null`: above zero | no |
| `max_duration_secs` | `integer \| null`: zero refuses live streams and accepts nothing else | no |
| `max_height` | `integer \| null`: above zero | no |
| `max_capture_secs` | `integer \| null`: how long a live stream is captured, above zero | no |

#### PlatformToggles

| Field | Type | Required |
|---|---|---|
| `default` | `PlatformDefault`: ignored while `presets` is non-empty | no, `inherit` |
| `presets` | `string[]`: preset ids. When any, the whitelist: platforms in any chosen preset are on, all others off | no |
| `overrides` | `object` of platform id → `bool`: win over presets and the default | no |

#### FrontendInput

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `slug` | `string`: lower-case letters, digits and dashes | yes |
| `description` | `string` | no |
| `enabled` | `bool` | no, `true` |
| `profile_id` | `uuid`: the platforms shown | no, the built-in profile |
| `scope` | `ContentScope` | no, everything |
| `access` | `Access` | no, closed with no way in |
| `downloads` | `bool` | no, `true` |
| `links` | `LinkPolicy` | no, off |

#### ContentScope

| Field | Type | Required |
|---|---|---|
| `guilds` | `snowflake[]` | no |
| `channels` | `snowflake[]` | no |

Includes jobs from any listed server or channel. Both lists empty includes all jobs.

#### Access

| Field | Type | Required |
|---|---|---|
| `open` | `bool`: everyone gets in | no |
| `secret_kind` | `"pin" \| "password" \| "token" \| null`: how the shared secret is asked for | no |
| `accounts` | `bool`: the view's own accounts may log in | no |
| `providers` | `string[]`: login provider ids | no |
| `discord_members` | `bool`: a Discord login must belong to every guild in the scope, counting the guild of every listed channel | no |
| `discord_users` | `snowflake[]`: a Discord login must be one of these | no |

#### LinkPolicy

| Field | Type | Required |
|---|---|---|
| `enabled` | `bool` | no, `false` |
| `min_height` | `integer`: pixels | no, `720` |
| `min_bitrate` | `integer`: bits per second | no, `1500000` |
| `max_bytes` | `integer`: bound of the output made for the page | no, 2 GiB |
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
| `exemplar` | `object`, the settings with every optional section filled with placeholder values, so the shape and type of every key can be read where `defaults` holds `null` |
| `entries` | `SettingEntry[]` |
| `secrets` | `string[]`, the secret keys that hold a value |
| `secret_keys` | `string[]`, every secret key, set or not |
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
| `builtin` | `bool`: ships with the server, cannot be removed |
| `server_limits` | `ServerLimits`: the engine's limits, which cap this profile |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

#### ServerLimits

| Field | Type |
|---|---|
| `max_source_bytes` | `integer` |
| `max_duration_secs` | `integer \| null`: `null` puts no bound on how long media may be |
| `max_height` | `integer` |
| `max_capture_secs` | `integer`: how long a live stream is captured at most |

#### Preset

| Field | Type |
|---|---|
| `id` | `string`: `basic`, `sfw`, `nsfw`, `news`, `social`, `video`, `music`, `podcasts`, `live`, `files`, `images` or `players` |
| `label` | `string` |
| `description` | `string` |
| `platforms` | `string[]`: resolver ids in it |

#### Scope

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"global" \| "guild" \| "channel" \| "user"` | all |
| `guild_id` | `snowflake` | `guild` `channel` `user` |
| `channel_id` | `snowflake` | `channel` |
| `user_id` | `snowflake` | `user` |

`ScopeKey` encodes a scope as `global`, `guild:<guild>`, `channel:<guild>:<channel>` or `user:<guild>:<user>`.

#### Assignment

| Field | Type |
|---|---|
| `scope` | `Scope` |
| `profile_id` | `uuid` |
| `updated_at` | `timestamp` |

#### EffectiveProfile

| Field | Type |
|---|---|
| `platforms` | `object` of platform id → `bool`: every platform, on or off |
| `limits` | `RequestLimits`: effective profile limits. `null` uses the engine limit |
| `audio_language` | `string \| null`: the language of the sound wanted, from the narrowest profile that names one |
| `applied` | `Assignment[]`: the assignments applied, widest first |

#### EffectiveView

| Field | Type |
|---|---|
| `...EffectiveProfile` | `EffectiveProfile` |
| `disabled` | `string[]`: the platform ids turned off |

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
| `subject` | `string`: `secret`, `account:<username>` or `provider:<id>:<subject>` |
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
| `platforms` | `string[]`: resolver ids shown |
| `viewer` | `{ frontend_id, subject, display } \| null` |

#### FrontPage

| Field | Type |
|---|---|
| `jobs` | `FrontJob[]` |
| `next` | `timestamp \| null`: the `before` of the next page |

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
| `live` | `bool`: a recorded stream |
| `recording` | `bool`: the media is the recording of a capture under way, playing while it grows |
| `size` | `integer` |
| `width` | `integer \| null` |
| `height` | `integer \| null` |
| `content_type` | `string` |
| `published_at` | `timestamp` |
| `media_url` | `string`: plays or shows the media, with its signed token |
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

#### GuildApplication

| Field | Type |
|---|---|
| `application_id` | `uuid` |
| `name` | `string` |
| `guild_name` | `string` |
| `present` | `bool` |

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
| `place` | `Place \| null`: the origin in the names people know, for a request from Discord |
| `destination` | `string \| null` |
| `submitted_by` | `string \| null` |
| `parent` | `uuid \| null` |
| `retry_of` | `uuid \| null` |
| `title` | `string \| null` |
| `resolver` | `string \| null` |
| `media` | `MediaKind`: what the probe found the source to be, else what the resolver said, else `video` |
| `uploader` | `string \| null` |
| `webpage_url` | `url \| null` |
| `thumbnail` | `url \| null` |
| `duration_secs` | `number \| null` |
| `live` | `bool` |
| `recording` | `bool`: a live capture is being recorded, or was |
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
| `guild` | `snowflake \| null`: the Discord server the link was seen in |
| `channel` | `snowflake \| null`: the Discord channel the link was seen in |

#### Place

Where on Discord a job came from and where its result goes, as the application's bot has
learned the names over its gateway since the server started. Each part is `null` until the
bot has seen it.

| Field | Type |
|---|---|
| `guild` | `PlaceGuild \| null` |
| `channel` | `PlaceChannel \| null`: where the link was posted |
| `destination` | `PlaceChannel \| null`: where the result is posted, when a rule sends it to another channel |
| `author` | `GuildMember \| null`: who posted the link |

#### PlaceGuild

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `icon` | `string \| null`: the icon hash on Discord's CDN |

#### PlaceChannel

| Field | Type |
|---|---|
| `id` | `snowflake` |
| `name` | `string` |
| `kind` | `ChannelKind` |

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
| `file` | `LocalFile`: the recording a live capture writes from its first byte | `recording` |

#### Progress

| Field | Type |
|---|---|
| `done` | `integer` |
| `total` | `integer \| null` |
| `bytes` | `integer \| null`: bytes on disk so far, for a capture whose recording grows as the stream goes on |

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
| `max_capture_secs` | `integer \| null`: how long a live stream is captured |

#### RequestOptions

| Field | Type |
|---|---|
| `clip` | `ClipRange \| null` |
| `subtitles` | `SubtitleMode` |
| `subtitle_language` | `string \| null` |
| `audio_language` | `string`: the language of the sound wanted when a source offers several, default `en`. Its track is taken when there is one, else the source's original |

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
| `place` | `Place \| null`: the request's origin in the names people know, for a request from Discord |
| `limits_in_force` | `LimitsInForce`: the request's limits tightened by the engine's own |
| `status` | `JobStatus` |
| `artifacts` | `Artifacts` |
| `log` | `LogEntry[]` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |
| `started_at` | `timestamp \| null` |
| `finished_at` | `timestamp \| null` |

#### LimitsInForce

Each field is the tighter of what the request named and the engine's cap.

| Field | Type |
|---|---|
| `max_source_bytes` | `integer` |
| `max_duration_secs` | `integer \| null`: `null` puts no bound on how long media may be, `0` refuses live streams |
| `max_height` | `integer` |
| `max_capture_secs` | `integer`: how long a live stream is captured at most |

#### Artifacts

| Field | Type |
|---|---|
| `resolved` | `Resolved \| null` |
| `recording` | `LocalFile \| null`: the recording a live capture writes from its first byte, playable while it grows |
| `announced` | `Published \| null`: the message the destination got when the capture began, edited with the result |
| `source` | `LocalFile \| null` |
| `output` | `LocalFile \| null` |
| `delivery` | `"upload" \| "link"`: whether the output was handed over or a view's page was posted |
| `link_reason` | `string \| null`: why a link was posted rather than the file |
| `published` | `Published \| null` |
| `archived` | `ArchiveEntry \| null` |
| `subtitles` | `LocalSubtitle[]` |
| `children` | `uuid[]` |
| `timings` | `StageTiming[]` |

#### Resolved

| Field | Type |
|---|---|
| `resolver` | `string` |
| `media` | `MediaKind`: what the link is. Decides how it is picked, shrunk and shown |
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
| `audio_track` | `string \| null`: the platform's own name for the audio track |
| `audio_default` | `bool`: the platform marks the track as the original its player takes by default |
| `audio_dubbed` | `bool`: the platform marks the track as a dub |
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
| `kind` | `MediaKind`: a still image has its picture in `video` with no `fps` |
| `duration` | `Duration \| null` |
| `video` | `VideoTrack \| null` |
| `audio` | `AudioTrack \| null` |
| `cover` | `AttachedPicture \| null` |
| `subtitles` | `EmbeddedSubtitle[]` |

#### VideoTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `width` | `integer` |
| `height` | `integer` |
| `fps` | `number \| null` |
| `bitrate` | `integer \| null` |
| `index` | `integer`: the stream's index in the file |
| `pix_fmt` | `string \| null` |
| `color` | `ColorInfo` |
| `hdr` | `HdrFormat \| null` |
| `field_order` | `FieldOrder` |
| `sample_aspect` | `[integer, integer] \| null`: the pixel aspect ratio when pixels are not square |
| `vfr` | `bool`: frames arrive at varying intervals |
| `alpha` | `bool`: the pixel format carries transparency |
| `projection` | `Projection \| null` |
| `stereo` | `StereoLayout \| null` |
| `view` | `[number, number, number] \| null`: yaw, pitch and roll of a 360° picture's initial view in degrees |

#### ColorInfo

| Field | Type |
|---|---|
| `primaries` | `string \| null` |
| `transfer` | `string \| null` |
| `matrix` | `string \| null` |
| `range` | `string \| null` |

#### HdrFormat

| Field | Type | Present for `format` |
|---|---|---|
| `format` | `pq` `hlg` `dolby_vision` | all |
| `profile` | `integer` | `dolby_vision` |

#### FieldOrder

| Value | Meaning |
|---|---|
| `unknown` | the stream does not say |
| `progressive` | whole frames |
| `top_first` | interlaced, top field first |
| `bottom_first` | interlaced, bottom field first |

#### Projection

| Field | Type | Present for `layout` |
|---|---|---|
| `layout` | `equirectangular` `cubemap` `equi_angular_cubemap` `equirectangular_tile` | all |
| `padding` | `integer` | `cubemap` |
| `left` `top` `right` `bottom` | `number`: fractions of the full sphere | `equirectangular_tile` |

#### StereoLayout

| Value | Meaning |
|---|---|
| `side_by_side` | two eyes side by side in one frame |
| `top_bottom` | two eyes one above the other |

#### AudioTrack

| Field | Type |
|---|---|
| `codec` | `Codec` |
| `channels` | `integer` |
| `sample_rate` | `integer` |
| `bitrate` | `integer \| null` |
| `index` | `integer`: the stream's index in the file |
| `language` | `string \| null` |

#### AttachedPicture

| Field | Type |
|---|---|
| `index` | `integer` |
| `width` | `integer` |
| `height` | `integer` |

#### EmbeddedSubtitle

| Field | Type |
|---|---|
| `index` | `integer` |
| `codec` | `string` |
| `language` | `string \| null` |
| `name` | `string \| null` |
| `bitmap` | `bool`: pictures rather than text |
| `default` | `bool` |
| `forced` | `bool` |

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
| `media` | `MediaKind[]`: every kind its links can resolve to |
| `tags` | `string[]`: what kind of place it is: the preset ids minus `sfw` |
| `session` | `SessionSupport` |
| `fixtures` | `FixtureResult[]` |
| `last_run_at` | `timestamp \| null` |
| `last_pass_at` | `timestamp \| null`: the last run in which every link passed |
| `last_fail_at` | `timestamp \| null` |
| `passed` | `integer` |
| `failed` | `integer` |
| `login_required` | `integer`: links that resolve only with a login the saved cookies do not give |
| `running` | `bool` |
| `cookies` | `integer`: cookies the app has saved for the platform |
| `cookies_updated_at` | `timestamp \| null`: when those cookies were last replaced or cleared |
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
| `last_pass_at` | `timestamp \| null`: when this link last resolved |
| `error` | `string \| null`: what went wrong, as a sentence that never repeats `url` |
| `title` | `string \| null` |
| `found` | `Found \| null`: what the link resolved to, when it did |
| `duration_ms` | `integer \| null` |

#### Found

| Field | Type | Present for `kind` |
|---|---|---|
| `kind` | `"media" \| "playlist"` | all |
| `media` | `MediaKind` | `media` |
| `variants` | `integer`: playable variants | `media` |
| `entries` | `integer` | `playlist` |

#### MediaKind

Media kinds: `"video"` (including animated GIFs), `"audio"`, `"image"` and `"file"`.

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

#### Healthz

| Field | Type |
|---|---|
| `status` | `ok` `warn` `fail` `stopping` |
| `version` | `string` |
| `at` | `timestamp` |

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
| `encoder` | the video encoders the settings chose run on this machine |
| `fonts` | a line of subtitles renders with a font on this machine |
| `bots` | no bot has failed or is retrying or lacks a token |
| `fixtures` | no platform has a failing fixture |
| `retention` | the last sweep went through |
| `backups` | the newest backup is not older than twice the interval and the last run did not fail |

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
| `transcode` | `TranscodeMetrics` |
| `retention` | `RetentionStatus` |
| `backups` | `BackupMetrics` |

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

#### TranscodeMetrics

| Field | Type |
|---|---|
| `ffmpeg` | `string`: the first line of `ffmpeg -version` |
| `source` | `embedded` `external` |
| `path` | `string \| null`: the path of an external build |
| `choice` | `EncoderChoice` |
| `hardware` | `EncoderChoice \| null`: the family in use, never `auto` or `software` |
| `h264_encoder` | `string \| null` |
| `shortfall` | `string \| null`: why the choice is not in use |

#### EncoderChoice

| Value | Meaning |
|---|---|
| `auto` | the first hardware family that runs here, else software |
| `software` | libx264 and its kin |
| `nvenc` | NVIDIA NVENC |
| `vaapi` | VA-API |
| `qsv` | Intel Quick Sync Video |
| `videotoolbox` | Apple VideoToolbox |
| `amf` | AMD AMF |
| `v4l2m2m` | Video4Linux memory-to-memory |

#### BackupMetrics

| Field | Type |
|---|---|
| `enabled` | `bool` |
| `count` | `integer` |
| `bytes` | `integer`: of every backup kept |
| `newest_at` | `timestamp \| null` |
| `last_error` | `string \| null` |
| `runs` | `integer` |

#### BackupsView

| Field | Type |
|---|---|
| `enabled` | `bool` |
| `dir` | `string` |
| `interval_secs` | `integer` |
| `keep` | `integer` |
| `status` | `BackupStatus` |
| `backups` | `BackupEntry[]`: newest first |

#### BackupEntry

| Field | Type |
|---|---|
| `name` | `string`: `discoclip-<UTC time>-<unique ID>.db`; older names without the ID remain supported |
| `bytes` | `integer` |
| `at` | `timestamp` |

#### RestoreStatus

| Field | Type |
|---|---|
| `id` | `uuid`: unguessable restore receipt |
| `phase` | `stopping \| restoring \| starting \| complete \| failed` |
| `error` | `string \| null` |

#### BackupStatus

| Field | Type |
|---|---|
| `last_at` | `timestamp \| null` |
| `last_bytes` | `integer \| null` |
| `last_error` | `string \| null` |
| `runs` | `integer` |

#### RetentionView

| Field | Type |
|---|---|
| `config` | `RetentionConfig` |
| `status` | `RetentionStatus` |

#### RetentionConfig

| Field | Type |
|---|---|
| `jobs_days` | `integer`: `0` keeps done jobs forever |
| `failed_jobs_days` | `integer`: `0` keeps failed and cancelled jobs forever |
| `cache_max_bytes` | `integer`: `0` never trims |
| `sweep_interval_secs` | `integer` |

#### RetentionStatus

| Field | Type |
|---|---|
| `last` | `SweepReport \| null` |
| `sweeps` | `integer` |
| `jobs_removed_total` | `integer` |
| `bytes_freed_total` | `integer` |

#### SweepReport

| Field | Type |
|---|---|
| `at` | `timestamp` |
| `jobs_removed` | `integer`: done jobs removed for their age |
| `failed_removed` | `integer`: failed and cancelled jobs removed for their age |
| `bytes_freed` | `integer` |
| `error` | `string \| null`: what went wrong along the way |
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
| `profile.create` | `converted_from_rule` | `object`: watch-rule migration metadata: `rule_id`, `application_id`, `guild_id`, `channel_id`, `allow_hosts`, `unmatched_hosts` (hosts using `web`), `max_source_bytes`, `max_duration_secs`, `max_height` |
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
| `stop`: ends the live captures among the jobs, keeping their recordings |
| `delete` |

#### Artifact

| Value |
|---|
| `output` |
| `source` |
| `subtitle` |
| `recording`: the recording of a live capture, served while it grows |

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
| `recording`: a live capture began |
| `stop`: a person asked the live capture to stop |
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
| `rtp` |
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
| `inherit` | Platforms the profile does not name stay as the parent scope has them |
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
| `backup` |

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
| `backup.run` |
| `backup.delete` |

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
