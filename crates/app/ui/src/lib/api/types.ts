// Wire types, one for one with API.md.

export type Role = 'admin' | 'operator' | 'viewer';

export type Permission =
	| 'manage_users'
	| 'manage_applications'
	| 'manage_watch_rules'
	| 'manage_bots'
	| 'view_audit_log';

export type Intent = 'login' | 'link';

export type CallbackError =
	| 'state'
	| 'denied'
	| 'provider'
	| 'identity'
	| 'exchange'
	| 'session'
	| 'already_linked'
	| 'provider_linked'
	| 'unknown_identity';

export type CommandMode = 'off' | 'global' | 'guilds';

export type BotStateName = 'disabled' | 'stopped' | 'starting' | 'connected' | 'retrying' | 'failed';

export type ActorKind = 'user' | 'provisioning';

export type Via = 'session' | 'token';

export type TargetKind = 'setting' | 'application' | 'rule';

export type Action =
	| 'settings.set'
	| 'settings.reset'
	| 'settings.import'
	| 'settings.provision'
	| 'application.create'
	| 'application.update'
	| 'application.delete'
	| 'application.commands.set'
	| 'application.commands.register'
	| 'bot.start'
	| 'bot.stop'
	| 'bot.restart'
	| 'rule.create'
	| 'rule.update'
	| 'rule.delete';

export const ROLES: Role[] = ['admin', 'operator', 'viewer'];

export const PERMISSIONS: Permission[] = [
	'manage_users',
	'manage_applications',
	'manage_watch_rules',
	'manage_bots',
	'view_audit_log'
];

export const COMMAND_MODES: CommandMode[] = ['off', 'global', 'guilds'];

export const BOT_STATES: BotStateName[] = [
	'disabled',
	'stopped',
	'starting',
	'connected',
	'retrying',
	'failed'
];

export const TARGET_KINDS: TargetKind[] = ['setting', 'application', 'rule'];

export const ACTIONS: Action[] = [
	'settings.set',
	'settings.reset',
	'settings.import',
	'settings.provision',
	'application.create',
	'application.update',
	'application.delete',
	'application.commands.set',
	'application.commands.register',
	'bot.start',
	'bot.stop',
	'bot.restart',
	'rule.create',
	'rule.update',
	'rule.delete'
];

// Request schemas

export interface SetupRequest {
	username: string;
	password: string;
	token: string;
}

export interface LoginRequest {
	username: string;
	password: string;
}

export interface UserCreateRequest {
	username: string;
	password?: string;
	role: Role;
}

export interface UserUpdateRequest {
	role: Role;
}

export interface PasswordRequest {
	password: string;
	current_password?: string;
}

export interface TokenCreateRequest {
	name: string;
	scopes?: Permission[];
	expires_in_days?: number;
}

export interface ApplicationCreateRequest {
	name?: string;
	bot_token: string;
	client_secret?: string;
}

export interface ApplicationUpdateRequest {
	name?: string;
	bot_token?: string;
	client_secret?: string | null;
	login?: boolean;
}

export interface CommandScope {
	mode: CommandMode;
	guilds?: string[];
}

export interface RuleInput {
	channel_id: string;
	post_to?: string | null;
	allow_hosts?: string[];
	allow_users?: string[];
	allow_roles?: string[];
	max_source_bytes?: number | null;
	max_duration_secs?: number | null;
	max_height?: number | null;
	enabled?: boolean;
}

export interface AuditQuery {
	actor?: string;
	action?: Action;
	target_kind?: TargetKind;
	target_id?: string;
	since?: string;
	until?: string;
	limit?: number;
	before?: string;
}

// Response schemas

export interface SetupStatus {
	needed: boolean;
}

export interface WhoAmI {
	user: User;
	csrf_token: string | null;
	session: SessionView | null;
	token: ApiToken | null;
}

export interface SessionView {
	id: string;
	created_at: string;
	last_seen_at: string;
	expires_at: string;
	user_agent: string | null;
	ip: string | null;
	current: boolean;
}

export interface Revoked {
	revoked: number;
}

export interface User {
	id: string;
	username: string;
	role: Role;
	has_password: boolean;
	created_at: string;
	updated_at: string;
}

export interface ApiToken {
	id: string;
	user_id: string;
	name: string;
	prefix: string;
	scopes: Permission[];
	created_at: string;
	last_used_at: string | null;
	expires_at: string | null;
}

export interface Minted {
	token: ApiToken;
	secret: string;
}

export interface ProviderInfo {
	id: string;
	name: string;
}

export interface Identity {
	id: string;
	user_id: string;
	provider: string;
	subject: string;
	username: string | null;
	display_name: string | null;
	email: string | null;
	scope: string | null;
	expires_at: string | null;
	has_refresh_token: boolean;
	linked_at: string;
	updated_at: string;
}

export interface Unlinked {
	identity: Identity;
	revoked: boolean;
}

export interface CommandsState {
	mode: CommandMode;
	guilds: string[];
	registered_at: string | null;
	error: string | null;
}

export interface ApplicationView {
	id: string;
	name: string;
	client_id: string;
	login: boolean;
	has_client_secret: boolean;
	commands: CommandsState;
	enabled: boolean;
	created_at: string;
	updated_at: string;
	bot: BotStatus;
	install_url: string;
}

export interface CommandSummary {
	name: string;
	description: string;
}

export interface CommandsView extends CommandsState {
	commands: CommandSummary[];
}

export interface InstallLink {
	url: string;
	scopes: string[];
	permissions: string[];
}

export type BotStatus = { since: string } & (
	| { state: 'disabled' }
	| { state: 'stopped' }
	| { state: 'starting' }
	| { state: 'connected'; user: string }
	| { state: 'retrying'; error: string; attempt: number; next_attempt_at: string }
	| { state: 'failed'; error: string }
);

export type BotEvent = BotStatus & { application: string };

export interface BotGuild {
	application_id: string;
	guild_id: string;
	name: string;
	icon: string | null;
	member_count: number | null;
	present: boolean;
	joined_at: string;
	left_at: string | null;
	updated_at: string;
}

export interface Rule {
	id: string;
	application_id: string;
	guild_id: string;
	channel_id: string;
	post_to: string | null;
	allow_hosts: string[];
	allow_users: string[];
	allow_roles: string[];
	max_source_bytes: number | null;
	max_duration_secs: number | null;
	max_height: number | null;
	enabled: boolean;
	created_at: string;
	updated_at: string;
}

export interface Guild {
	id: string;
	name: string;
	icon: string | null;
	owner: boolean;
	permissions: string;
	manageable: boolean;
	fetched_at: string;
}

export interface Page {
	entries: Entry[];
	next: string | null;
}

export interface Entry {
	id: string;
	at: string;
	actor: Actor;
	action: Action;
	target: Target;
	details: Record<string, unknown>;
}

export type Actor =
	| { kind: 'user'; id: string; username: string; via: Via; ip: string }
	| { kind: 'provisioning'; file: string | null };

export interface Target {
	kind: TargetKind;
	id: string;
	name: string | null;
}
