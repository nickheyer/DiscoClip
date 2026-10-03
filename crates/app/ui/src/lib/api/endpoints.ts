// One function per endpoint in API.md, grouped as the reference groups them.

import { del, fileUrl, get, patch, post, put, type Query, type RequestOptions } from './client';
import type * as T from './types';

const enc = encodeURIComponent;

// ---- Setup and login ----

export const auth = {
	setupStatus: () => get<T.SetupStatus>('/setup'),
	setup: (body: T.SetupRequest) => post<T.WhoAmI>('/setup', body),
	recover: (body: T.RecoverRequest) =>
		post<T.WhoAmI>('/recover', body, undefined, { expectUnauthorized: true }),
	login: (body: T.LoginRequest) =>
		post<T.WhoAmI>('/login', body, undefined, { expectUnauthorized: true }),
	logout: () => post<void>('/logout'),
	session: (options?: RequestOptions) => get<T.WhoAmI>('/session', undefined, options)
};

// ---- Sessions ----

export const sessions = {
	list: () => get<T.SessionView[]>('/sessions'),
	listAll: () => get<T.AccountSessionView[]>('/sessions/all'),
	revokeOthers: () => del<T.Revoked>('/sessions/others'),
	revoke: (id: T.Uuid) => del<void>(`/sessions/${enc(id)}`)
};

// ---- Roles ----

export const roles = {
	list: () => get<T.RoleView[]>('/roles')
};

// ---- Users ----

export const users = {
	list: () => get<T.User[]>('/users'),
	create: (body: T.UserCreateRequest) => post<T.User>('/users', body),
	get: (id: T.Uuid) => get<T.User>(`/users/${enc(id)}`),
	update: (id: T.Uuid, body: T.UserUpdateRequest) => patch<T.User>(`/users/${enc(id)}`, body),
	remove: (id: T.Uuid) => del<void>(`/users/${enc(id)}`),
	setPassword: (id: T.Uuid, body: T.PasswordRequest) =>
		put<void>(`/users/${enc(id)}/password`, body),
	sessions: (id: T.Uuid) => get<T.SessionView[]>(`/users/${enc(id)}/sessions`),
	revokeSessions: (id: T.Uuid) => del<T.Revoked>(`/users/${enc(id)}/sessions`),
	revokeSession: (id: T.Uuid, session: T.Uuid) =>
		del<void>(`/users/${enc(id)}/sessions/${enc(session)}`),
	tokens: (id: T.Uuid) => get<T.ApiToken[]>(`/users/${enc(id)}/tokens`),
	revokeToken: (id: T.Uuid, token: T.Uuid) => del<void>(`/users/${enc(id)}/tokens/${enc(token)}`)
};

// ---- API tokens ----

export const tokens = {
	list: () => get<T.ApiToken[]>('/tokens'),
	create: (body: T.TokenCreateRequest) => post<T.Minted>('/tokens', body),
	listAll: () => get<T.AccountTokenView[]>('/tokens/all'),
	revoke: (id: T.Uuid) => del<void>(`/tokens/${enc(id)}`)
};

// ---- Login providers ----

export const providers = {
	list: () => get<T.ProviderInfo[]>('/auth/providers'),
	/** Where the browser goes to log in or link through a provider. */
	startUrl: (provider: string, intent: T.Intent = 'login') =>
		fileUrl(`/auth/${enc(provider)}/start`, { intent }),
	identities: () => get<T.Identity[]>('/auth/identities'),
	unlink: (provider: string) => del<T.Unlinked>(`/auth/identities/${enc(provider)}`),
	refresh: (provider: string) => post<T.Identity>(`/auth/identities/${enc(provider)}/refresh`)
};

// ---- Settings ----

export const settings = {
	get: () => get<T.SettingsView>('/settings'),
	change: (body: T.SettingsChange) => patch<T.SettingsView>('/settings', body),
	set: (key: string, value: T.Json) =>
		put<T.SettingsView>(`/settings/${enc(key)}`, { value } satisfies T.SettingSetRequest),
	reset: (key: string) => del<T.SettingsView>(`/settings/${enc(key)}`),
	import: (body: T.SettingsImportRequest) => post<T.SettingsView>('/settings/import', body),
	exportUrl: (format: T.SettingsFormat) => fileUrl('/settings/export', { format })
};

// ---- Discord applications ----

const app = (id: T.Uuid) => `/discord/applications/${enc(id)}`;
const guildOf = (id: T.Uuid, guild: T.Snowflake) => `${app(id)}/guilds/${enc(guild)}`;

export const applications = {
	list: () => get<T.ApplicationView[]>('/discord/applications'),
	create: (body: T.ApplicationCreateRequest) =>
		post<T.ApplicationView>('/discord/applications', body),
	get: (id: T.Uuid) => get<T.ApplicationView>(app(id)),
	update: (id: T.Uuid, body: T.ApplicationUpdateRequest) => patch<T.ApplicationView>(app(id), body),
	remove: (id: T.Uuid) => del<void>(app(id)),
	install: (id: T.Uuid, guild?: T.Snowflake) => get<T.InstallLink>(`${app(id)}/install`, { guild }),
	guilds: (id: T.Uuid) => get<T.BotGuild[]>(`${app(id)}/guilds`),
	channels: (id: T.Uuid, guild: T.Snowflake) =>
		get<T.GuildChannel[]>(`${guildOf(id, guild)}/channels`),
	roles: (id: T.Uuid, guild: T.Snowflake) => get<T.GuildRole[]>(`${guildOf(id, guild)}/roles`),
	members: (id: T.Uuid, guild: T.Snowflake, q: string, limit = 20) =>
		get<T.GuildMember[]>(`${guildOf(id, guild)}/members`, { q, limit }),
	member: (id: T.Uuid, guild: T.Snowflake, user: T.Snowflake) =>
		get<T.GuildMember>(`${guildOf(id, guild)}/members/${enc(user)}`),
	// Slash command registration
	commands: (id: T.Uuid) => get<T.CommandsView>(`${app(id)}/commands`),
	setCommands: (id: T.Uuid, body: T.CommandScope) =>
		put<T.CommandsView>(`${app(id)}/commands`, body),
	register: (id: T.Uuid) => post<T.CommandsView>(`${app(id)}/commands/register`),
	// Bots
	start: (id: T.Uuid) => post<T.ApplicationView>(`${app(id)}/bot/start`),
	stop: (id: T.Uuid) => post<T.ApplicationView>(`${app(id)}/bot/stop`),
	restart: (id: T.Uuid) => post<T.ApplicationView>(`${app(id)}/bot/restart`)
};

// ---- Watch rules ----

export const rules = {
	forGuild: (id: T.Uuid, guild: T.Snowflake) => get<T.Rule[]>(`${guildOf(id, guild)}/rules`),
	create: (id: T.Uuid, guild: T.Snowflake, body: T.RuleInput) =>
		post<T.Rule>(`${guildOf(id, guild)}/rules`, body),
	list: () => get<T.RuleView[]>('/discord/rules'),
	get: (id: T.Uuid) => get<T.Rule>(`/discord/rules/${enc(id)}`),
	update: (id: T.Uuid, body: T.RuleInput) => put<T.Rule>(`/discord/rules/${enc(id)}`, body),
	remove: (id: T.Uuid) => del<void>(`/discord/rules/${enc(id)}`)
};

// ---- Profiles ----

export const profiles = {
	list: () => get<T.Profile[]>('/profiles'),
	create: (body: T.ProfileInput) => post<T.Profile>('/profiles', body),
	get: (id: T.Uuid) => get<T.Profile>(`/profiles/${enc(id)}`),
	update: (id: T.Uuid, body: T.ProfileInput) => put<T.Profile>(`/profiles/${enc(id)}`, body),
	remove: (id: T.Uuid) => del<void>(`/profiles/${enc(id)}`),
	presets: () => get<T.Preset[]>('/profiles/presets'),
	assignments: (guild?: T.Snowflake) => get<T.Assignment[]>('/profiles/assignments', { guild }),
	assign: (scope: T.ScopeKey, profile_id: T.Uuid) =>
		put<T.Assignment>(`/profiles/assignments/${enc(scope)}`, {
			profile_id
		} satisfies T.AssignRequest),
	unassign: (scope: T.ScopeKey) => del<void>(`/profiles/assignments/${enc(scope)}`),
	effective: (query: { guild?: T.Snowflake; channel?: T.Snowflake; user?: T.Snowflake } = {}) =>
		get<T.EffectiveView>('/profiles/effective', query)
};

/** The string a scope is addressed by. */
export function scopeKey(scope: T.Scope): T.ScopeKey {
	switch (scope.kind) {
		case 'global':
			return 'global';
		case 'guild':
			return `guild:${scope.guild_id}`;
		case 'channel':
			return `channel:${scope.guild_id}:${scope.channel_id}`;
		case 'user':
			return `user:${scope.guild_id}:${scope.user_id}`;
	}
}

// ---- Content views ----

const view = (id: T.Uuid) => `/frontends/${enc(id)}`;

export const frontends = {
	list: () => get<T.Frontend[]>('/frontends'),
	create: (body: T.FrontendInput) => post<T.Frontend>('/frontends', body),
	get: (id: T.Uuid) => get<T.Frontend>(view(id)),
	update: (id: T.Uuid, body: T.FrontendInput) => put<T.Frontend>(view(id), body),
	remove: (id: T.Uuid) => del<void>(view(id)),
	setSecret: (id: T.Uuid, secret: string | null) =>
		put<T.Frontend>(`${view(id)}/secret`, { secret } satisfies T.FrontendSecretRequest),
	users: (id: T.Uuid) => get<T.FrontendUser[]>(`${view(id)}/users`),
	createUser: (id: T.Uuid, body: T.FrontendUserCreateRequest) =>
		post<T.FrontendUser>(`${view(id)}/users`, body),
	setUserPassword: (id: T.Uuid, user: T.Uuid, password: string) =>
		put<T.FrontendUser>(`${view(id)}/users/${enc(user)}/password`, {
			password
		} satisfies T.FrontendUserPasswordRequest),
	removeUser: (id: T.Uuid, user: T.Uuid) => del<void>(`${view(id)}/users/${enc(user)}`),
	sessions: (id: T.Uuid) => get<T.ViewerSession[]>(`${view(id)}/sessions`),
	revokeSessions: (id: T.Uuid) => del<void>(`${view(id)}/sessions`),
	revokeSession: (id: T.Uuid, session: T.Uuid) => del<void>(`${view(id)}/sessions/${enc(session)}`)
};

// ---- Content view visitors ----

const slugOf = (slug: string) => `/f/${enc(slug)}`;

export const front = {
	info: (slug: string) => get<T.FrontInfo>(slugOf(slug)),
	login: (slug: string, body: T.FrontLoginRequest) =>
		post<T.FrontInfo>(`${slugOf(slug)}/login`, body, undefined, { expectUnauthorized: true }),
	logout: (slug: string) => post<void>(`${slugOf(slug)}/logout`),
	providerStartUrl: (slug: string, provider: string) =>
		fileUrl(`${slugOf(slug)}/auth/${enc(provider)}/start`),
	jobs: (slug: string, query: T.FrontJobQuery = {}) =>
		get<T.FrontPage>(`${slugOf(slug)}/jobs`, query as Query, { expectUnauthorized: true }),
	job: (slug: string, id: T.Uuid) =>
		get<T.FrontJob>(`${slugOf(slug)}/jobs/${enc(id)}`, undefined, { expectUnauthorized: true }),
	mediaUrl: (slug: string, id: T.Uuid) => fileUrl(`${slugOf(slug)}/jobs/${enc(id)}/media`),
	downloadUrl: (slug: string, id: T.Uuid) => fileUrl(`${slugOf(slug)}/jobs/${enc(id)}/download`)
};

// ---- Account guilds ----

export const guilds = {
	/** The applications whose bots have been in a guild the account manages. */
	applications: (guild: T.Snowflake) =>
		get<T.GuildApplication[]>(`/discord/guilds/${enc(guild)}/applications`)
};

// ---- Jobs ----

export const jobs = {
	list: (query: T.JobQuery = {}) => get<T.JobPage>('/jobs', query as Query),
	submit: (body: T.SubmitRequest) => post<T.Submitted>('/jobs', body),
	bulk: (body: T.BulkRequest) => post<T.BulkResponse>('/jobs/bulk', body),
	stats: () => get<T.JobStats>('/jobs/stats'),
	get: (id: T.Uuid) => get<T.Job>(`/jobs/${enc(id)}`),
	remove: (id: T.Uuid) => del<void>(`/jobs/${enc(id)}`),
	retry: (id: T.Uuid) => post<T.Submitted>(`/jobs/${enc(id)}/retry`),
	cancel: (id: T.Uuid) => post<void>(`/jobs/${enc(id)}/cancel`),
	/** Ends a running live capture, keeping what was recorded. */
	stop: (id: T.Uuid) => post<void>(`/jobs/${enc(id)}/stop`),
	downloadUrl: (
		id: T.Uuid,
		query: { artifact?: T.Artifact; index?: number; inline?: boolean } = {}
	) => fileUrl(`/jobs/${enc(id)}/download`, query),
	children: (id: T.Uuid) => get<T.JobSummary[]>(`/jobs/${enc(id)}/children`)
};

/** The server-sent event feeds. */
export const streams = {
	/** Job statistics, job events and bot status on one connection. */
	events: () => fileUrl('/events'),
	/** Every new log line that matches the filter. */
	logs: (query: Omit<T.LogQuery, 'before' | 'limit'> = {}) =>
		fileUrl('/logs/events', query as Query)
};

// ---- Audit log ----

export const audit = {
	list: (query: T.AuditQuery = {}) => get<T.Page>('/audit', query as Query)
};

// ---- Platforms ----

export const platforms = {
	list: () => get<T.PlatformCoverage[]>('/platforms'),
	get: (id: string) => get<T.PlatformCoverage>(`/platforms/${enc(id)}`),
	checkAll: () => post<T.CheckStarted>('/platforms/check'),
	check: (id: string) => post<T.PlatformCoverage>(`/platforms/${enc(id)}/check`),
	// Check links
	addLink: (id: string, body: T.FixtureLinkRequest) =>
		post<T.PlatformCoverage>(`/platforms/${enc(id)}/fixtures`, body),
	changeLink: (id: string, link: T.Uuid, body: T.FixtureLinkChange) =>
		patch<T.PlatformCoverage>(`/platforms/${enc(id)}/fixtures/${enc(link)}`, body),
	removeLink: (id: string, link: T.Uuid) =>
		del<T.PlatformCoverage>(`/platforms/${enc(id)}/fixtures/${enc(link)}`),
	checkLink: (id: string, link: T.Uuid) =>
		post<T.PlatformCoverage>(`/platforms/${enc(id)}/fixtures/${enc(link)}/check`),

	// Platform sessions
	setCookies: (id: string, body: T.CookiesImport) =>
		put<T.SessionOutcome>(`/platforms/${enc(id)}/cookies`, body),
	clearCookies: (id: string) => del<T.PlatformCoverage>(`/platforms/${enc(id)}/cookies`),
	checkSession: (id: string) => post<T.PlatformCoverage>(`/platforms/${enc(id)}/session/check`)
};

// ---- Health and metrics ----

export const health = {
	get: () => get<T.Health>('/health')
};

export const metrics = {
	get: () => get<T.Metrics>('/metrics')
};

// ---- Backups and retention ----

export const backups = {
	list: () => get<T.BackupsView>('/backups'),
	run: () => post<T.BackupEntry>('/backups'),
	restore: (name: string) => post<T.RestoreStatus>(`/backups/${enc(name)}/restore`),
	downloadUrl: (name: string) => fileUrl(`/backups/${enc(name)}`),
	remove: (name: string) => del<void>(`/backups/${enc(name)}`)
};

export const retention = {
	get: () => get<T.RetentionView>('/retention'),
	sweep: () => post<T.SweepReport>('/retention/sweep')
};

// ---- Server log ----

export const logs = {
	list: (query: T.LogQuery = {}) => get<T.LogPage>('/logs', query as Query)
};
