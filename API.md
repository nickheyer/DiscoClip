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
| Origin check | `Origin` equal to `Host` |
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
| `manage_watch_rules` | yes | yes | no |
| `manage_bots` | yes | yes | no |

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
| `500` | `{ "error": "internal error" }` |
| `502` | `{ "error": "<message>" }` |

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

#### Revoked

| Field | Type |
|---|---|
| `revoked` | `integer` |

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

#### Rule

| Field | Type |
|---|---|
| `id` | `uuid` |
| `application_id` | `uuid` |
| `guild_id` | `snowflake` |
| `...RuleInput` | `RuleInput` |
| `created_at` | `timestamp` |
| `updated_at` | `timestamp` |

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

#### Intent

| Value |
|---|
| `login` |
| `link` |

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

#### TargetKind

| Value |
|---|
| `setting` |
| `application` |
| `rule` |

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
