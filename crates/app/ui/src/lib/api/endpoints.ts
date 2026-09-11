// Every endpoint in API.md, as a function.

import { del, get, patch, post, put } from './client';
import type {
	ApiToken,
	ApplicationCreateRequest,
	ApplicationUpdateRequest,
	ApplicationView,
	AuditQuery,
	BotGuild,
	CommandScope,
	CommandsView,
	Guild,
	Identity,
	InstallLink,
	Intent,
	LoginRequest,
	Minted,
	Page,
	PasswordRequest,
	ProviderInfo,
	Revoked,
	Rule,
	RuleInput,
	SessionView,
	SetupRequest,
	SetupStatus,
	TokenCreateRequest,
	Unlinked,
	User,
	UserCreateRequest,
	UserUpdateRequest,
	WhoAmI
} from './types';

const id = encodeURIComponent;

export const auth = {
	setupStatus: () => get<SetupStatus>('/setup'),
	setup: (body: SetupRequest) => post<WhoAmI>('/setup', body),
	login: (body: LoginRequest) => post<WhoAmI>('/login', body, [401]),
	logout: () => post<void>('/logout', undefined, [401]),
	session: () => get<WhoAmI>('/session', undefined, [401]),
	sessions: () => get<SessionView[]>('/sessions'),
	revokeOtherSessions: () => del<Revoked>('/sessions/others'),
	revokeSession: (session: string) => del<void>(`/sessions/${id(session)}`)
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
	tokens: (user: string) => get<ApiToken[]>(`/users/${id(user)}/tokens`),
	revokeToken: (user: string, token: string) =>
		del<void>(`/users/${id(user)}/tokens/${id(token)}`)
};

export const tokens = {
	list: () => get<ApiToken[]>('/tokens'),
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

export const guilds = {
	list: () => get<Guild[]>('/discord/guilds'),
	refresh: () => post<Guild[]>('/discord/guilds/refresh')
};

export const audit = {
	list: (query: AuditQuery) => get<Page>('/audit', { ...query })
};
