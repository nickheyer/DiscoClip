// Every endpoint in API.md, as a function.

import { del, get, patch, post, publicRequest, put, text } from './client';
import type {
	AccountSessionView,
	AccountTokenView,
	ApiToken,
	ApplicationCreateRequest,
	ApplicationUpdateRequest,
	ApplicationView,
	Artifact,
	Assignment,
	AuditQuery,
	BotGuild,
	BulkRequest,
	BulkResponse,
	CheckStarted,
	CommandScope,
	CommandsView,
	CookiesImport,
	EffectiveView,
	FrontInfo,
	FrontJob,
	FrontJobQuery,
	FrontLogin,
	FrontPage,
	Frontend,
	FrontendInput,
	FrontendUser,
	Guild,
	GuildChannel,
	GuildMember,
	GuildRole,
	Health,
	Identity,
	InstallLink,
	Intent,
	Job,
	JobPage,
	JobQuery,
	JobStats,
	JobSummary,
	LogPage,
	LogQuery,
	LoginRequest,
	Metrics,
	Minted,
	Page,
	PasswordRequest,
	PlatformCoverage,
	Preset,
	Profile,
	ProfileInput,
	ProviderInfo,
	Revoked,
	RoleView,
	Rule,
	RuleInput,
	Scope,
	SessionOutcome,
	SessionView,
	SettingValue,
	SettingsChange,
	SettingsFormat,
	SettingsImportRequest,
	SettingsView,
	SetupRequest,
	SetupStatus,
	SubmitRequest,
	Submitted,
	TokenCreateRequest,
	Unlinked,
	User,
	UserCreateRequest,
	UserUpdateRequest,
	ViewerSession,
	WhoAmI
} from './types';

const id = encodeURIComponent;

/** The live feed of job stats, job events and bot statuses; a plain URL for an EventSource. */
export const LIVE_EVENTS_URL = '/api/events';

export const auth = {
	setupStatus: () => get<SetupStatus>('/setup'),
	setup: (body: SetupRequest) => post<WhoAmI>('/setup', body),
	login: (body: LoginRequest) => post<WhoAmI>('/login', body, [401]),
	logout: () => post<void>('/logout', undefined, [401]),
	session: () => get<WhoAmI>('/session', undefined, [401]),
	sessions: () => get<SessionView[]>('/sessions'),
	allSessions: () => get<AccountSessionView[]>('/sessions/all'),
	revokeOtherSessions: () => del<Revoked>('/sessions/others'),
	revokeSession: (session: string) => del<void>(`/sessions/${id(session)}`)
};

export const roles = {
	list: () => get<RoleView[]>('/roles')
};

export const users = {
	list: () => get<User[]>('/users'),
	create: (body: UserCreateRequest) => post<User>('/users', body),
	get: (user: string) => get<User>(`/users/${id(user)}`),
	update: (user: string, body: UserUpdateRequest) => patch<User>(`/users/${id(user)}`, body),
	remove: (user: string) => del<void>(`/users/${id(user)}`),
	setPassword: (user: string, body: PasswordRequest) =>
		put<void>(`/users/${id(user)}/password`, body),
	sessions: (user: string) => get<SessionView[]>(`/users/${id(user)}/sessions`),
	revokeSessions: (user: string) => del<Revoked>(`/users/${id(user)}/sessions`),
	revokeSession: (user: string, session: string) =>
		del<void>(`/users/${id(user)}/sessions/${id(session)}`),
	tokens: (user: string) => get<ApiToken[]>(`/users/${id(user)}/tokens`),
	revokeToken: (user: string, token: string) =>
		del<void>(`/users/${id(user)}/tokens/${id(token)}`)
};

export const tokens = {
	list: () => get<ApiToken[]>('/tokens'),
	all: () => get<AccountTokenView[]>('/tokens/all'),
	create: (body: TokenCreateRequest) => post<Minted>('/tokens', body),
	revoke: (token: string) => del<void>(`/tokens/${id(token)}`)
};

export const providers = {
	list: () => get<ProviderInfo[]>('/auth/providers'),
	/** The browser is sent here with a full page load; the server answers with a redirect. */
	startUrl: (provider: string, intent: Intent) =>
		`/api/auth/${id(provider)}/start?intent=${intent}`,
	identities: () => get<Identity[]>('/auth/identities'),
	unlink: (provider: string) => del<Unlinked>(`/auth/identities/${id(provider)}`),
	refresh: (provider: string) => post<Identity>(`/auth/identities/${id(provider)}/refresh`)
};

export const applications = {
	list: () => get<ApplicationView[]>('/discord/applications'),
	create: (body: ApplicationCreateRequest) =>
		post<ApplicationView>('/discord/applications', body),
	get: (application: string) => get<ApplicationView>(`/discord/applications/${id(application)}`),
	update: (application: string, body: ApplicationUpdateRequest) =>
		patch<ApplicationView>(`/discord/applications/${id(application)}`, body),
	remove: (application: string) => del<void>(`/discord/applications/${id(application)}`),
	install: (application: string, guild?: string) =>
		get<InstallLink>(`/discord/applications/${id(application)}/install`, { guild }),
	guilds: (application: string) =>
		get<BotGuild[]>(`/discord/applications/${id(application)}/guilds`),
	commands: (application: string) =>
		get<CommandsView>(`/discord/applications/${id(application)}/commands`),
	setCommands: (application: string, scope: CommandScope) =>
		put<CommandsView>(`/discord/applications/${id(application)}/commands`, scope),
	register: (application: string) =>
		post<CommandsView>(`/discord/applications/${id(application)}/commands/register`),
	startBot: (application: string) =>
		post<ApplicationView>(`/discord/applications/${id(application)}/bot/start`),
	stopBot: (application: string) =>
		post<ApplicationView>(`/discord/applications/${id(application)}/bot/stop`),
	restartBot: (application: string) =>
		post<ApplicationView>(`/discord/applications/${id(application)}/bot/restart`),
	eventsUrl: '/api/discord/bots/events'
};

export const channels = {
	list: (application: string, guild: string) =>
		get<GuildChannel[]>(`/discord/applications/${id(application)}/guilds/${id(guild)}/channels`),
	roles: (application: string, guild: string) =>
		get<GuildRole[]>(`/discord/applications/${id(application)}/guilds/${id(guild)}/roles`),
	members: (application: string, guild: string, q: string) =>
		get<GuildMember[]>(`/discord/applications/${id(application)}/guilds/${id(guild)}/members`, {
			q
		}),
	member: (application: string, guild: string, user: string) =>
		get<GuildMember>(
			`/discord/applications/${id(application)}/guilds/${id(guild)}/members/${id(user)}`
		)
};

export const rules = {
	listForGuild: (application: string, guild: string) =>
		get<Rule[]>(`/discord/applications/${id(application)}/guilds/${id(guild)}/rules`),
	createForGuild: (application: string, guild: string, body: RuleInput) =>
		post<Rule>(`/discord/applications/${id(application)}/guilds/${id(guild)}/rules`, body),
	listAll: () => get<Rule[]>('/discord/rules'),
	get: (rule: string) => get<Rule>(`/discord/rules/${id(rule)}`),
	update: (rule: string, body: RuleInput) => put<Rule>(`/discord/rules/${id(rule)}`, body),
	remove: (rule: string) => del<void>(`/discord/rules/${id(rule)}`)
};

/** The one string a scope is addressed by in the API. */
export function scopeKey(scope: Scope): string {
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

export const profiles = {
	list: () => get<Profile[]>('/profiles'),
	/** The presets a profile can choose, each with the platforms in it. */
	presets: () => get<Preset[]>('/profiles/presets'),
	get: (profile: string) => get<Profile>(`/profiles/${id(profile)}`),
	create: (body: ProfileInput) => post<Profile>('/profiles', body),
	update: (profile: string, body: ProfileInput) => put<Profile>(`/profiles/${id(profile)}`, body),
	remove: (profile: string) => del<void>(`/profiles/${id(profile)}`),
	/** The whole server's assignment and, with a guild, that guild's; every guild's without. */
	assignments: (guild?: string) =>
		get<Assignment[]>('/profiles/assignments', guild ? { guild } : {}),
	assign: (scope: Scope, profile: string) =>
		put<Assignment>(`/profiles/assignments/${id(scopeKey(scope))}`, { profile_id: profile }),
	unassign: (scope: Scope) => del<void>(`/profiles/assignments/${id(scopeKey(scope))}`),
	effective: (guild?: string, channel?: string, user?: string) => {
		const query: Record<string, string> = {};
		if (guild) query.guild = guild;
		if (channel) query.channel = channel;
		if (user) query.user = user;
		return get<EffectiveView>('/profiles/effective', query);
	}
};

export const frontends = {
	list: () => get<Frontend[]>('/frontends'),
	get: (frontend: string) => get<Frontend>(`/frontends/${id(frontend)}`),
	create: (body: FrontendInput) => post<Frontend>('/frontends', body),
	update: (frontend: string, body: FrontendInput) =>
		put<Frontend>(`/frontends/${id(frontend)}`, body),
	remove: (frontend: string) => del<void>(`/frontends/${id(frontend)}`),
	/** Stores the shared secret; `null` removes it. */
	setSecret: (frontend: string, secret: string | null) =>
		put<Frontend>(`/frontends/${id(frontend)}/secret`, { secret }),
	users: (frontend: string) => get<FrontendUser[]>(`/frontends/${id(frontend)}/users`),
	createUser: (frontend: string, username: string, password: string) =>
		post<FrontendUser>(`/frontends/${id(frontend)}/users`, { username, password }),
	setUserPassword: (frontend: string, user: string, password: string) =>
		put<FrontendUser>(`/frontends/${id(frontend)}/users/${id(user)}/password`, { password }),
	removeUser: (frontend: string, user: string) =>
		del<void>(`/frontends/${id(frontend)}/users/${id(user)}`),
	sessions: (frontend: string) => get<ViewerSession[]>(`/frontends/${id(frontend)}/sessions`),
	revokeSessions: (frontend: string) => del<void>(`/frontends/${id(frontend)}/sessions`),
	revokeSession: (frontend: string, session: string) =>
		del<void>(`/frontends/${id(frontend)}/sessions/${id(session)}`)
};

/** A front end as its visitors reach it: no admin session, no admin CSRF token. */
export const front = {
	info: (slug: string) => publicRequest<FrontInfo>('GET', `/f/${id(slug)}`),
	login: (slug: string, body: FrontLogin) =>
		publicRequest<FrontInfo>('POST', `/f/${id(slug)}/login`, { body }),
	logout: (slug: string) => publicRequest<void>('POST', `/f/${id(slug)}/logout`),
	/** The browser is sent here with a full page load; the server answers with a redirect. */
	startUrl: (slug: string, provider: string) => `/api/f/${id(slug)}/auth/${id(provider)}/start`,
	jobs: (slug: string, query: FrontJobQuery = {}) =>
		publicRequest<FrontPage>('GET', `/f/${id(slug)}/jobs`, { query: { ...query } }),
	job: (slug: string, job: string) => publicRequest<FrontJob>('GET', `/f/${id(slug)}/jobs/${id(job)}`),
	/** Where the media page lives, for sharing. */
	pagePath: (slug: string, job: string) => `/f/${id(slug)}/j/${id(job)}`
};

export const guilds = {
	list: () => get<Guild[]>('/discord/guilds'),
	refresh: () => post<Guild[]>('/discord/guilds/refresh')
};

export const audit = {
	list: (query: AuditQuery) => get<Page>('/audit', { ...query })
};

export const health = {
	get: () => get<Health>('/health')
};

export const metrics = {
	get: () => get<Metrics>('/metrics')
};

export const logs = {
	list: (query: LogQuery = {}) => get<LogPage>('/logs', { ...query }),
	/** Where the browser follows new lines; a plain URL for an EventSource, not an API call. */
	eventsUrl: (query: Pick<LogQuery, 'level' | 'target' | 'q'>) => {
		const params = new URLSearchParams();
		for (const [key, value] of Object.entries(query)) {
			if (value) params.set(key, String(value));
		}
		const text = params.toString();
		return `/api/logs/events${text ? `?${text}` : ''}`;
	}
};

export const platforms = {
	list: () => get<PlatformCoverage[]>('/platforms'),
	get: (platform: string) => get<PlatformCoverage>(`/platforms/${id(platform)}`),
	checkAll: () => post<CheckStarted>('/platforms/check'),
	check: (platform: string) => post<PlatformCoverage>(`/platforms/${id(platform)}/check`),
	importCookies: (platform: string, body: CookiesImport) =>
		put<SessionOutcome>(`/platforms/${id(platform)}/cookies`, body),
	clearCookies: (platform: string) => del<PlatformCoverage>(`/platforms/${id(platform)}/cookies`),
	checkSession: (platform: string) =>
		post<PlatformCoverage>(`/platforms/${id(platform)}/session/check`)
};

export const settings = {
	get: () => get<SettingsView>('/settings'),
	change: (body: SettingsChange) => patch<SettingsView>('/settings', body),
	set: (key: string, value: SettingValue) =>
		put<SettingsView>(`/settings/${id(key)}`, { value }),
	reset: (key: string) => del<SettingsView>(`/settings/${id(key)}`),
	import: (body: SettingsImportRequest) => post<SettingsView>('/settings/import', body),
	exportText: (format: SettingsFormat) => text(`/settings/export?format=${format}`),
	/** Where the browser downloads the settings as a file; a plain link, not an API call. */
	exportUrl: (format: SettingsFormat) => `/api/settings/export?format=${format}`
};

export const jobs = {
	list: (query: JobQuery = {}) => get<JobPage>('/jobs', { ...query }),
	get: (job: string) => get<Job>(`/jobs/${id(job)}`),
	children: (job: string) => get<JobSummary[]>(`/jobs/${id(job)}/children`),
	stats: () => get<JobStats>('/jobs/stats'),
	submit: (body: SubmitRequest) => post<Submitted>('/jobs', body),
	retry: (job: string) => post<Submitted>(`/jobs/${id(job)}/retry`),
	cancel: (job: string) => post<void>(`/jobs/${id(job)}/cancel`),
	remove: (job: string) => del<void>(`/jobs/${id(job)}`),
	bulk: (body: BulkRequest) => post<BulkResponse>('/jobs/bulk', body),
	/** Where the browser fetches or plays an artifact; a plain link, not an API call. */
	downloadUrl: (job: string, artifact: Artifact = 'output', index = 0, inline = false) =>
		`/api/jobs/${id(job)}/download?artifact=${artifact}&index=${index}${inline ? '&inline=true' : ''}`,
	eventsUrl: '/api/jobs/events'
};
