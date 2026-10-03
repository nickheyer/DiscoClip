// The wire types of API.md, named as the reference names them.

// ---- Scalars ----

/** A UUID as a string. */
export type Uuid = string;
/** An RFC 3339 timestamp. */
export type Timestamp = string;
/** A Discord snowflake as a decimal string. */
export type Snowflake = string;
export type Url = string;
export type Ip = string;
export type Json = unknown;

// ---- Enumerations ----

export type Role = 'admin' | 'operator' | 'viewer';

export type Permission =
	| 'manage_users'
	| 'manage_applications'
	| 'manage_watch_rules'
	| 'manage_bots'
	| 'view_audit_log'
	| 'manage_jobs'
	| 'manage_settings'
	| 'view_logs';

/** What each role is allowed, from the reference's permission table. */
export const ROLE_PERMISSIONS: Record<Role, readonly Permission[]> = {
	admin: [
		'manage_users',
		'manage_applications',
		'view_audit_log',
		'manage_settings',
		'view_logs',
		'manage_watch_rules',
		'manage_bots',
		'manage_jobs'
	],
	operator: ['manage_watch_rules', 'manage_bots', 'manage_jobs'],
	viewer: []
};

/** What each permission is called where people read it. */
export const PERMISSION_LABELS: Record<Permission, string> = {
	manage_users: 'Manage users',
	manage_applications: 'Manage applications',
	manage_watch_rules: 'Manage watch rules',
	manage_bots: 'Start and stop bots',
	view_audit_log: 'View audit log',
	manage_jobs: 'Manage jobs',
	manage_settings: 'Manage settings',
	view_logs: 'View logs'
};

export const PERMISSIONS: readonly Permission[] = ROLE_PERMISSIONS.admin;

export type Intent = 'login' | 'link';
export type SettingSource = 'provisioning' | 'app';
export type SettingsFormat = 'toml' | 'yaml' | 'json';
export type StatusKind = 'queued' | 'running' | 'done' | 'failed' | 'cancelled';
export type Stage = 'resolve' | 'download' | 'transcode' | 'publish' | 'archive';
export type BulkAction = 'retry' | 'cancel' | 'stop' | 'delete';
export type Artifact = 'output' | 'source' | 'subtitle' | 'recording';
export type JobOrder = 'newest' | 'oldest';
export type JobEventKind =
	'submitted' | 'status' | 'progress' | 'log' | 'children' | 'recording' | 'stop' | 'deleted';
export type SubtitleMode = 'keep' | 'burn' | 'skip';
export type SubtitleFormat = 'vtt' | 'srt' | 'ttml' | 'ass' | 'json3' | 'hls_vtt';
export type VariantKind =
	'file' | 'hls' | 'dash' | 'ism' | 'rtmp' | 'rtsp' | 'rtp' | 'whep' | 'browser';
export type CipherScheme = 'aes128_ctr';
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
export type ChannelKind =
	'text' | 'announcement' | 'voice' | 'stage' | 'category' | 'forum' | 'media' | 'thread' | 'other';
export type SessionSupport = 'none' | 'optional' | 'required';
export type MediaKind = 'video' | 'audio' | 'image' | 'file';
export type PlatformDefault = 'inherit' | 'enabled' | 'disabled';
export type BotState = 'disabled' | 'stopped' | 'starting' | 'connected' | 'retrying' | 'failed';
export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';
export type FixtureStatus = 'pass' | 'fail' | 'login_required' | 'never';
/** Where a check link came from: shipped with the resolver, added by hand, or a finished job's. */
export type LinkOrigin = 'builtin' | 'custom' | 'job';
/** Whether a platform works, by its check links and the jobs that finished on it. */
export type PlatformHealth = 'working' | 'failing' | 'login_required' | 'unknown';
export type CookieFormat = 'netscape' | 'header';
export type HealthStatus = 'ok' | 'warn' | 'fail';
export type SessionState = 'unsupported' | 'logged_out' | 'logged_in';
export type ActorKind = 'user' | 'provisioning';
export type Via = 'session' | 'token';
export type TargetKind =
	'setting' | 'application' | 'rule' | 'platform' | 'profile' | 'frontend' | 'backup';
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
	| 'rule.delete'
	| 'session.import'
	| 'session.clear'
	| 'profile.create'
	| 'profile.update'
	| 'profile.delete'
	| 'profile.assign'
	| 'profile.unassign'
	| 'frontend.create'
	| 'frontend.update'
	| 'frontend.delete'
	| 'frontend.secret.set'
	| 'frontend.secret.clear'
	| 'frontend.user.create'
	| 'frontend.user.password'
	| 'frontend.user.delete'
	| 'frontend.sessions.revoke'
	| 'backup.run'
	| 'backup.delete'
	| 'backup.restore';
export type SecretKind = 'pin' | 'password' | 'token';
export type PresetId =
	| 'basic'
	| 'sfw'
	| 'nsfw'
	| 'news'
	| 'social'
	| 'video'
	| 'music'
	| 'podcasts'
	| 'live'
	| 'files'
	| 'images'
	| 'players';

// ---- Request schemas ----

export interface SetupRequest {
	username: string;
	password: string;
}

export interface RecoverRequest {
	username: string;
	/** The recovery key from the server console. */
	key: string;
	/** The new password. */
	password: string;
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
	/** Required when an account changes its own password. */
	current_password?: string;
}

export interface TokenCreateRequest {
	name: string;
	scopes?: Permission[];
	expires_in_days?: number;
}

export interface SettingsChange {
	set?: Record<string, Json>;
	reset?: string[];
}

export interface SettingSetRequest {
	value: Json;
}

export interface SettingsImportRequest {
	format: SettingsFormat;
	text: string;
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
	guilds?: Snowflake[];
}

export interface RuleInput {
	/** The channel watched, or `null` for every channel of the server. */
	channel_id: Snowflake | null;
	post_to?: Snowflake | null;
	allow_users?: Snowflake[];
	allow_roles?: Snowflake[];
	enabled?: boolean;
}

export interface ProfileInput {
	name: string;
	description?: string;
	platforms?: PlatformToggles;
	limits?: ProfileLimits;
	/** The language of the sound taken when a source offers several. Unset leaves the parent scope's. */
	audio_language?: string | null;
}

export interface ProfileLimits {
	/** Above zero. */
	max_source_bytes?: number | null;
	/** Zero refuses live streams and accepts nothing else. */
	max_duration_secs?: number | null;
	/** Above zero. */
	max_height?: number | null;
	/** How long a live stream is captured, in seconds. Above zero. */
	max_capture_secs?: number | null;
}

export interface PlatformToggles {
	/** Ignored while `presets` is non-empty. */
	default?: PlatformDefault;
	/** With any, the whitelist: platforms in any chosen preset are on, all others off. */
	presets?: string[];
	/** Win over presets and the default. */
	overrides?: Record<string, boolean>;
}

export interface FrontendInput {
	name: string;
	/** Lower-case letters, digits and dashes. */
	slug: string;
	description?: string;
	enabled?: boolean;
	/** The platforms shown. */
	profile_id?: Uuid;
	scope?: ContentScope;
	access?: Access;
	downloads?: boolean;
	links?: LinkPolicy;
}

/** Both lists empty includes all jobs. */
export interface ContentScope {
	guilds?: Snowflake[];
	channels?: Snowflake[];
}

export interface Access {
	/** Everyone gets in. */
	open?: boolean;
	/** How the shared secret is asked for. */
	secret_kind?: SecretKind | null;
	/** The view's own accounts may log in. */
	accounts?: boolean;
	/** Login provider ids. */
	providers?: string[];
	/** A Discord login must belong to every guild in the scope. */
	discord_members?: boolean;
	/** A Discord login must be one of these. */
	discord_users?: Snowflake[];
}

export interface LinkPolicy {
	enabled?: boolean;
	/** Pixels. */
	min_height?: number;
	/** Bits per second. */
	min_bitrate?: number;
	/** Bound of the output made for the page. */
	max_bytes?: number;
	signed_link_days?: number;
}

export interface SubmitRequest {
	url: Url;
	limits?: RequestLimits;
	options?: RequestOptions;
}

export interface BulkRequest {
	action: BulkAction;
	/** 1 to 500. */
	ids: Uuid[];
}

export interface JobQuery {
	source?: string;
	status?: StatusKind;
	resolver?: string;
	parent?: Uuid;
	top_level?: boolean;
	q?: string;
	before?: Timestamp;
	after?: Timestamp;
	/** Default 50, max 500. */
	limit?: number;
	offset?: number;
	order?: JobOrder;
}

export interface CookiesImport {
	format: CookieFormat;
	text: string;
	/** `header` only. Defaults to the platform's first host. */
	domain?: string;
}

export interface LogQuery {
	level?: LogLevel;
	target?: string;
	q?: string;
	before?: number;
	/** Default 200, max 1000. */
	limit?: number;
}

export interface AuditQuery {
	actor?: Uuid;
	action?: Action;
	target_kind?: TargetKind;
	/** With `target_kind`. */
	target_id?: string;
	since?: Timestamp;
	until?: Timestamp;
	/** Default 50, max 500. */
	limit?: number;
	before?: Uuid;
}

export interface FrontJobQuery {
	q?: string;
	media?: MediaKind;
	resolver?: string;
	before?: Timestamp;
	/** At most 48. */
	limit?: number;
}

export interface FrontSecretLogin {
	secret: string;
}

export interface FrontAccountLogin {
	username: string;
	password: string;
}

export type FrontLoginRequest = FrontSecretLogin | FrontAccountLogin;

export interface FrontendUserCreateRequest {
	username: string;
	password: string;
}

export interface FrontendUserPasswordRequest {
	password: string;
}

export interface FrontendSecretRequest {
	secret: string | null;
}

export interface AssignRequest {
	profile_id: Uuid;
}

// ---- Response schemas ----

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
	id: Uuid;
	created_at: Timestamp;
	last_seen_at: Timestamp;
	expires_at: Timestamp;
	user_agent: string | null;
	ip: Ip | null;
	current: boolean;
}

export interface AccountSessionView extends SessionView {
	user_id: Uuid;
	username: string;
}

export interface Revoked {
	revoked: number;
}

export interface RoleView {
	role: Role;
	description: string;
	permissions: Permission[];
	accounts: User[];
}

export interface User {
	id: Uuid;
	username: string;
	role: Role;
	has_password: boolean;
	created_at: Timestamp;
	updated_at: Timestamp;
}

export interface ApiToken {
	id: Uuid;
	user_id: Uuid;
	name: string;
	prefix: string;
	scopes: Permission[];
	created_at: Timestamp;
	last_used_at: Timestamp | null;
	expires_at: Timestamp | null;
}

export interface AccountTokenView extends ApiToken {
	username: string;
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
	id: Uuid;
	user_id: Uuid;
	provider: string;
	subject: string;
	username: string | null;
	display_name: string | null;
	email: string | null;
	scope: string | null;
	expires_at: Timestamp | null;
	has_refresh_token: boolean;
	linked_at: Timestamp;
	updated_at: Timestamp;
}

export interface Unlinked {
	identity: Identity;
	revoked: boolean;
}

export interface SettingsView {
	settings: Record<string, Json>;
	defaults: Record<string, Json>;
	/** The settings with every optional section filled with placeholder values. */
	exemplar: Record<string, Json>;
	entries: SettingEntry[];
	/** The secret keys that hold a value. */
	secrets: string[];
	/** Every secret key, set or not. */
	secret_keys: string[];
	data_dir: string;
	provisioning_file: string | null;
	/** The address browsers reach the app at, as links and login callbacks are built on it. */
	public_url: string | null;
	public_url_source: 'configured' | 'learned' | null;
}

export interface SettingEntry {
	key: string;
	source: SettingSource;
	updated_at: Timestamp;
}

export interface ApplicationView {
	id: Uuid;
	name: string;
	client_id: Snowflake;
	login: boolean;
	has_client_secret: boolean;
	commands: CommandsState;
	enabled: boolean;
	created_at: Timestamp;
	updated_at: Timestamp;
	bot: BotStatus;
	install_url: Url;
	login_callback_url: Url;
}

export interface CommandsState {
	mode: CommandMode;
	guilds: Snowflake[];
	registered_at: Timestamp | null;
	error: string | null;
}

export interface CommandsView extends CommandsState {
	commands: CommandSummary[];
}

export interface CommandSummary {
	name: string;
	description: string;
}

export interface InstallLink {
	url: Url;
	scopes: string[];
	permissions: string[];
}

export type BotStatus = { since: Timestamp } & (
	| { state: 'disabled' | 'stopped' | 'starting' }
	| { state: 'connected'; user: string }
	| { state: 'retrying'; error: string; attempt: number; next_attempt_at: Timestamp }
	| { state: 'failed'; error: string }
);

export type BotEvent = BotStatus & {
	application: Uuid;
	/** Only when the application was removed: the last event about its bot. */
	removed?: true;
};

export interface BotGuild {
	application_id: Uuid;
	guild_id: Snowflake;
	name: string;
	icon: string | null;
	member_count: number | null;
	present: boolean;
	joined_at: Timestamp;
	left_at: Timestamp | null;
	updated_at: Timestamp;
}

export interface GuildChannel {
	id: Snowflake;
	name: string;
	kind: ChannelKind;
	parent_id: Snowflake | null;
	position: number;
	rule: Uuid | null;
}

export interface GuildRole {
	id: Snowflake;
	name: string;
	color: number;
	position: number;
	managed: boolean;
}

export interface GuildMember {
	id: Snowflake;
	username: string;
	display_name: string | null;
	nick: string | null;
	avatar: string | null;
	bot: boolean;
}

export interface Rule extends Required<RuleInput> {
	id: Uuid;
	application_id: Uuid;
	guild_id: Snowflake;
	created_at: Timestamp;
	updated_at: Timestamp;
}

/** A rule with the names of the places it points at, as far as the bots know them. */
export interface RuleView extends Rule {
	guild_name: string | null;
	guild_icon: string | null;
	channel_name: string | null;
	post_to_name: string | null;
}

export interface Profile extends Required<ProfileInput> {
	id: Uuid;
	/** Ships with the server, cannot be removed. */
	builtin: boolean;
	/** The engine's limits, which cap this profile. */
	server_limits: ServerLimits;
	created_at: Timestamp;
	updated_at: Timestamp;
}

/** The engine's own limits. They cap every profile, whatever a profile names. */
export interface ServerLimits {
	max_source_bytes: number;
	/** `null` puts no bound on how long media may be. */
	max_duration_secs: number | null;
	max_height: number;
	/** How long a live stream is captured at most, in seconds. */
	max_capture_secs: number;
}

export interface Preset {
	id: PresetId;
	label: string;
	description: string;
	/** Resolver ids in it. */
	platforms: string[];
}

export type Scope =
	| { kind: 'global' }
	| { kind: 'guild'; guild_id: Snowflake }
	| { kind: 'channel'; guild_id: Snowflake; channel_id: Snowflake }
	| { kind: 'user'; guild_id: Snowflake; user_id: Snowflake };

/** `global`, `guild:<guild>`, `channel:<guild>:<channel>` or `user:<guild>:<user>`. */
export type ScopeKey = string;

export interface Assignment {
	scope: Scope;
	profile_id: Uuid;
	updated_at: Timestamp;
}

export interface EffectiveProfile {
	/** Every platform, on or off. */
	platforms: Record<string, boolean>;
	/** Effective profile limits. `null` uses the engine limit. */
	limits: RequestLimits;
	/** The language of the sound wanted, from the narrowest profile that names one. */
	audio_language: string | null;
	/** The assignments applied, widest first. */
	applied: Assignment[];
}

export interface EffectiveView extends EffectiveProfile {
	/** The platform ids turned off. */
	disabled: string[];
}

export interface Frontend extends Required<FrontendInput> {
	id: Uuid;
	has_secret: boolean;
	created_at: Timestamp;
	updated_at: Timestamp;
}

export interface FrontendUser {
	id: Uuid;
	frontend_id: Uuid;
	username: string;
	created_at: Timestamp;
}

export interface ViewerSession {
	id: Uuid;
	frontend_id: Uuid;
	/** `secret`, `account:<username>` or `provider:<id>:<subject>`. */
	subject: string;
	display: string;
	created_at: Timestamp;
	last_seen_at: Timestamp;
	expires_at: Timestamp;
	ip: string | null;
	user_agent: string | null;
}

export interface FrontAccess {
	open: boolean;
	secret: SecretKind | null;
	accounts: boolean;
	providers: ProviderInfo[];
	discord_members: boolean;
}

export interface FrontViewer {
	frontend_id: Uuid;
	subject: string;
	display: string;
}

export interface FrontInfo {
	slug: string;
	name: string;
	description: string;
	downloads: boolean;
	access: FrontAccess;
	/** Resolver ids shown. */
	platforms: string[];
	viewer: FrontViewer | null;
}

export interface FrontPage {
	jobs: FrontJob[];
	/** The `before` of the next page. */
	next: Timestamp | null;
}

export interface FrontJob {
	id: Uuid;
	title: string | null;
	media: MediaKind;
	resolver: string;
	uploader: string | null;
	webpage_url: Url | null;
	/** Shows the still that stands for the media, with its signed token. */
	thumbnail: string | null;
	duration_secs: number | null;
	/** A recorded stream. */
	live: boolean;
	/** The media is the recording of a capture under way, playing while it grows. */
	recording: boolean;
	size: number;
	width: number | null;
	height: number | null;
	content_type: string;
	published_at: Timestamp;
	/** Plays or shows the media, with its signed token. */
	media_url: string;
	download_url: string | null;
}

export interface Submitted {
	id: Uuid;
}

export interface BulkResponse {
	action: BulkAction;
	results: BulkOutcome[];
	succeeded: number;
	failed: number;
}

export interface BulkOutcome {
	id: Uuid;
	ok: boolean;
	error: string | null;
	job: Uuid | null;
}

export interface JobPage {
	jobs: JobSummary[];
	total: number;
	limit: number;
	offset: number;
}

export interface JobSummary {
	id: Uuid;
	url: Url;
	status: JobStatus;
	source: string;
	origin: Origin;
	/** The origin in the names people know, for a request from Discord. */
	place: Place | null;
	destination: string | null;
	submitted_by: string | null;
	parent: Uuid | null;
	retry_of: Uuid | null;
	title: string | null;
	resolver: string | null;
	/** What the probe found the source to be, else what the resolver said, else `video`. */
	media: MediaKind;
	uploader: string | null;
	webpage_url: Url | null;
	/** Where the still that stands for the output is served, once the job has an output. */
	thumbnail: string | null;
	duration_secs: number | null;
	live: boolean;
	/** A live capture is being recorded, or was. */
	recording: boolean;
	output_bytes: number | null;
	published_url: Url | null;
	published_reference: string | null;
	children: number;
	archived_files: number;
	created_at: Timestamp;
	updated_at: Timestamp;
	started_at: Timestamp | null;
	finished_at: Timestamp | null;
}

export type JobStatus =
	| { status: 'queued' }
	| { status: 'running'; stage: Stage }
	| { status: 'done' }
	| { status: 'failed'; stage: Stage; message: string }
	| { status: 'cancelled' };

export interface Origin {
	source: string;
	reference: string;
	url: Url | null;
	/** The Discord server the link was seen in. */
	guild: Snowflake | null;
	/** The Discord channel the link was seen in. */
	channel: Snowflake | null;
}

/** A Discord server as a job's place names it. */
export interface PlaceGuild {
	id: Snowflake;
	name: string;
	icon: string | null;
}

/** A Discord channel as a job's place names it. */
export interface PlaceChannel {
	id: Snowflake;
	name: string;
	kind: ChannelKind;
}

/**
 * Where on Discord a job came from and where its result goes, in the names people know.
 * Each part is there when the application's bot has learned it over its gateway.
 */
export interface Place {
	guild: PlaceGuild | null;
	/** Where the link was posted. */
	channel: PlaceChannel | null;
	/** Where the result is posted, when a rule sends it to another channel. */
	destination: PlaceChannel | null;
	/** Who posted the link. */
	author: GuildMember | null;
}

export interface JobStats {
	counts: Stats;
	last_24h: Stats;
	utilisation: Utilisation;
	queue_depth: number;
	active: Uuid[];
	resolvers: ResolverStats[];
	at: Timestamp;
}

export interface Stats {
	queued: number;
	running: number;
	done: number;
	failed: number;
	cancelled: number;
}

export interface Utilisation {
	workers: number;
	active: number;
	waiting: number;
}

export interface ResolverStats {
	resolver: string;
	done: number;
	failed: number;
	last_done_at: Timestamp | null;
	last_failed_at: Timestamp | null;
}

export type JobEvent = { job: Uuid; at: Timestamp; job_summary: JobSummary | null } & (
	| { kind: 'submitted'; request: JobRequest }
	| { kind: 'status'; status: JobStatus }
	| { kind: 'progress'; stage: Stage; progress: Progress }
	| { kind: 'log'; entry: LogEntry }
	| { kind: 'children'; ids: Uuid[] }
	| { kind: 'recording'; file: LocalFile }
	| { kind: 'stop' }
	| { kind: 'deleted' }
);

export interface Progress {
	done: number;
	total: number | null;
	/** Bytes on disk so far, for a capture whose recording grows as the stream goes on. */
	bytes: number | null;
}

export interface LogEntry {
	at: Timestamp;
	stage: Stage | null;
	message: string;
}

export interface JobRequest {
	origin: Origin;
	url: Url;
	destination: string | null;
	limits: RequestLimits;
	options: RequestOptions;
	parent: Uuid | null;
	retry_of: Uuid | null;
	submitted_by: string | null;
}

export interface RequestLimits {
	max_source_bytes: number | null;
	max_duration_secs: number | null;
	max_height: number | null;
	/** How long a live stream is captured, in seconds. */
	max_capture_secs: number | null;
}

/** The limits a job runs under, each the tighter of its request's and the engine's cap. */
export interface LimitsInForce {
	max_source_bytes: number;
	/** `null` puts no bound on how long media may be; `0` refuses live streams. */
	max_duration_secs: number | null;
	max_height: number;
	/** How long a live stream is captured at most, in seconds. */
	max_capture_secs: number;
}

export interface RequestOptions {
	clip: ClipRange | null;
	subtitles: SubtitleMode;
	subtitle_language: string | null;
	/** The language of the sound wanted when a source offers several. */
	audio_language: string;
}

export interface ClipRange {
	start: Duration;
	end: Duration | null;
}

export interface Duration {
	secs: number;
	nanos: number;
}

export interface Job {
	id: Uuid;
	request: JobRequest;
	/** The request's origin in the names people know, for a request from Discord. */
	place: Place | null;
	/** The request's limits tightened by the engine's own: what the job is held to. */
	limits_in_force: LimitsInForce;
	status: JobStatus;
	artifacts: Artifacts;
	log: LogEntry[];
	created_at: Timestamp;
	updated_at: Timestamp;
	started_at: Timestamp | null;
	finished_at: Timestamp | null;
}

export interface Artifacts {
	resolved: Resolved | null;
	/** The recording a live capture writes from its first byte, playable while it grows. */
	recording: LocalFile | null;
	/** The message the destination got when the capture began, edited with the result. */
	announced: Published | null;
	source: LocalFile | null;
	output: LocalFile | null;
	/** Whether the output was handed over or a view's page was posted. */
	delivery: 'upload' | 'link';
	/** Why a link was posted rather than the file. */
	link_reason: string | null;
	published: Published | null;
	archived: ArchiveEntry | null;
	subtitles: LocalSubtitle[];
	children: Uuid[];
	timings: StageTiming[];
}

export interface Resolved {
	resolver: string;
	/** What the link is. Decides how it is picked, shrunk and shown. */
	media: MediaKind;
	id: string | null;
	title: string | null;
	description: string | null;
	uploader: string | null;
	uploader_url: Url | null;
	uploaded_at: Timestamp | null;
	duration: Duration | null;
	thumbnail: Url | null;
	webpage_url: Url | null;
	live: boolean;
	age_limit: number | null;
	clip: ClipRange | null;
	subtitles: SubtitleTrack[];
	variants: Variant[];
}

export interface Variant {
	url: Url;
	kind: VariantKind;
	audio_url: Url | null;
	container: Codec | null;
	video: Codec | null;
	audio: Codec | null;
	width: number | null;
	height: number | null;
	fps: number | null;
	bitrate: number | null;
	size: number | null;
	duration: Duration | null;
	headers: [string, string][];
	format_id: string | null;
	label: string | null;
	language: string | null;
	/** The platform's own name for the audio track. */
	audio_track: string | null;
	/** The platform marks the track as the original its player takes by default. */
	audio_default: boolean;
	/** The platform marks the track as a dub. */
	audio_dubbed: boolean;
	codecs: string | null;
	video_only: boolean;
	audio_only: boolean;
	live: boolean;
	drm: string | null;
	cipher: Cipher | null;
}

export type Cipher = { scheme: 'aes128_ctr'; key: number[]; nonce: number[] };

/** A known codec or container name, or a name outside the known set. */
export type Codec = string | { other: string };

export interface SubtitleTrack {
	url: Url;
	language: string;
	name: string | null;
	format: SubtitleFormat;
	auto: boolean;
	headers: [string, string][];
}

export interface LocalFile {
	path: string;
	size: number;
	info: MediaInfo | null;
}

export interface MediaInfo {
	container: Codec;
	/** A still image has its picture in `video` with no `fps`. */
	kind: MediaKind;
	duration: Duration | null;
	video: VideoTrack | null;
	audio: AudioTrack | null;
	/** Cover art or a poster frame carried beside the streams. */
	cover: AttachedPicture | null;
	/** The subtitle streams inside the file, in order. */
	subtitles: EmbeddedSubtitle[];
}

export type FieldOrder = 'unknown' | 'progressive' | 'top_first' | 'bottom_first';
export type HdrFormat =
	{ format: 'pq' } | { format: 'hlg' } | { format: 'dolby_vision'; profile: number };
export type Projection =
	| { layout: 'equirectangular' }
	| { layout: 'cubemap'; padding: number }
	| { layout: 'equi_angular_cubemap' }
	| { layout: 'equirectangular_tile'; left: number; top: number; right: number; bottom: number };
export type StereoLayout = 'side_by_side' | 'top_bottom';

export interface ColorInfo {
	primaries: string | null;
	transfer: string | null;
	matrix: string | null;
	range: string | null;
}

export interface VideoTrack {
	codec: Codec;
	width: number;
	height: number;
	fps: number | null;
	bitrate: number | null;
	/** The stream's index in the file. */
	index: number;
	pix_fmt: string | null;
	color: ColorInfo;
	/** The high dynamic range format, when the picture is not SDR. */
	hdr: HdrFormat | null;
	field_order: FieldOrder;
	/** The pixel aspect ratio as `[num, den]` when pixels are not square. */
	sample_aspect: [number, number] | null;
	/** Whether frames arrive at varying intervals. */
	vfr: boolean;
	/** Whether the pixel format carries transparency. */
	alpha: boolean;
	/** How a 360° picture is laid out, when it is one. */
	projection: Projection | null;
	stereo: StereoLayout | null;
	/** The initial view of a 360° picture as `[yaw, pitch, roll]` in degrees. */
	view: [number, number, number] | null;
}

export interface AudioTrack {
	codec: Codec;
	channels: number;
	sample_rate: number;
	bitrate: number | null;
	/** The stream's index in the file. */
	index: number;
	language: string | null;
}

export interface AttachedPicture {
	index: number;
	width: number;
	height: number;
}

export interface EmbeddedSubtitle {
	index: number;
	codec: string;
	language: string | null;
	name: string | null;
	/** Pictures rather than text. */
	bitmap: boolean;
	default: boolean;
	forced: boolean;
}

export interface LocalSubtitle {
	language: string;
	name: string | null;
	path: string;
	format: SubtitleFormat;
}

export interface Published {
	reference: string;
	url: Url | null;
	at: Timestamp;
}

export interface ArchiveEntry {
	files: string[];
	bytes: number;
	at: Timestamp;
}

export interface StageTiming {
	stage: Stage;
	started_at: Timestamp;
	ended_at: Timestamp | null;
}

/** A page of audit entries. */
export interface Page {
	entries: Entry[];
	next: Uuid | null;
}

/** An audit entry. */
export interface Entry {
	id: Uuid;
	at: Timestamp;
	actor: Actor;
	action: Action;
	target: Target;
	details: AuditDetails;
}

export interface PlatformCoverage {
	id: string;
	name: string;
	hosts: string[];
	features: string[];
	formats: string[];
	/** Every kind its links can resolve to. */
	media: MediaKind[];
	/** What kind of place it is: the preset ids minus `sfw`. */
	tags: string[];
	session: SessionSupport;
	fixtures: FixtureResult[];
	/** When a link of the platform was last run. */
	last_run_at: Timestamp | null;
	/** When a link of the platform last resolved. */
	last_pass_at: Timestamp | null;
	/** When a job last finished on the platform. */
	last_job_at: Timestamp | null;
	/** Of the links in use, how many last resolved. */
	passed: number;
	failed: number;
	/** Links in use that resolve only with a login the platform's saved cookies do not give. */
	login_required: number;
	health: PlatformHealth;
	running: boolean;
	/** How many cookies the app has saved for the platform. */
	cookies: number;
	cookies_updated_at: Timestamp | null;
	session_check: SessionCheckResult | null;
}

export type SessionCheckResult = { at: Timestamp } & (
	{ state: 'unsupported' | 'logged_out' } | { state: 'logged_in'; account: string }
);

export interface SessionOutcome extends PlatformCoverage {
	check_error: string | null;
}

export interface FixtureResult {
	id: Uuid;
	url: Url;
	origin: LinkOrigin;
	/** Whether runs try the link. */
	enabled: boolean;
	/** What the link failed with while another link resolved, when that is why it is off. */
	disabled_reason: string | null;
	status: FixtureStatus;
	run_at: Timestamp | null;
	last_pass_at: Timestamp | null;
	error: string | null;
	title: string | null;
	/** What the link resolved to, when it did. */
	found: Found | null;
	duration_ms: number | null;
}

export type Found =
	{ kind: 'media'; media: MediaKind; variants: number } | { kind: 'playlist'; entries: number };

export interface CheckStarted {
	platforms: string[];
}

export interface FixtureLinkRequest {
	url: string;
}

export interface FixtureLinkChange {
	url?: string;
	enabled?: boolean;
}

export interface Health {
	status: HealthStatus;
	version: string;
	started_at: Timestamp;
	uptime_secs: number;
	at: Timestamp;
	checks: HealthCheck[];
}

export interface HealthCheck {
	name: string;
	label: string;
	status: HealthStatus;
	detail: string;
	/** The page where what the check looks at is seen to. */
	href: string | null;
}

export interface Metrics {
	at: Timestamp;
	version: string;
	started_at: Timestamp;
	uptime_secs: number;
	process: ProcessMetrics | null;
	system: SystemMetrics;
	jobs: JobStats;
	http: HttpMetrics;
	bots: BotMetrics;
	cache: CacheMetrics;
	database: DatabaseMetrics;
	fixtures: FixtureMetrics;
	logs: LogMetrics;
	transcode: TranscodeMetrics;
	retention: RetentionStatus;
	backups: BackupMetrics;
}

export interface BackupMetrics {
	enabled: boolean;
	count: number;
	/** Bytes of every backup kept. */
	bytes: number;
	newest_at: Timestamp | null;
	last_error: string | null;
	runs: number;
}

export interface BackupEntry {
	name: string;
	bytes: number;
	/** When it was made, from its name. */
	at: Timestamp;
}

export interface RestoreStatus {
	id: string;
	phase: 'stopping' | 'restoring' | 'starting' | 'complete' | 'failed';
	error: string | null;
}

export interface BackupStatus {
	last_at: Timestamp | null;
	last_bytes: number | null;
	last_error: string | null;
	runs: number;
}

export interface BackupsView {
	enabled: boolean;
	dir: string;
	interval_secs: number;
	keep: number;
	status: BackupStatus;
	/** Newest first. */
	backups: BackupEntry[];
}

export interface SweepReport {
	at: Timestamp;
	jobs_removed: number;
	failed_removed: number;
	bytes_freed: number;
	/** What went wrong along the way. The other steps still ran. */
	error: string | null;
}

export interface RetentionStatus {
	last: SweepReport | null;
	sweeps: number;
	jobs_removed_total: number;
	bytes_freed_total: number;
}

export interface RetentionConfig {
	jobs_days: number;
	failed_jobs_days: number;
	cache_max_bytes: number;
	sweep_interval_secs: number;
}

export interface RetentionView {
	config: RetentionConfig;
	status: RetentionStatus;
}

export interface ProcessMetrics {
	pid: number;
	rss_bytes: number;
	virtual_bytes: number;
	cpu_percent: number;
	run_time_secs: number;
}

export interface SystemMetrics {
	total_memory_bytes: number;
	available_memory_bytes: number;
	load_average: [number, number, number];
	cpus: number;
	disks: DiskMetrics[];
}

export interface DiskMetrics {
	mount: string;
	total_bytes: number;
	available_bytes: number;
	holds: ('cache' | 'data' | 'local' | 'archive')[];
}

export interface HttpMetrics {
	requests: RequestCount[];
	retries: number;
	rate_limit_waits: number;
	bytes_received: number;
}

export interface RequestCount {
	host: string;
	status: number;
	count: number;
}

export interface BotMetrics {
	applications: number;
	by_state: Partial<Record<BotState, number>>;
}

export interface CacheMetrics {
	dir: string;
	bytes: number;
	jobs: number;
}

export interface DatabaseMetrics {
	path: string;
	bytes: number;
}

export interface FixtureMetrics {
	platforms: number;
	/** Platforms with a check link in use. */
	with_fixtures: number;
	working: number;
	failing: number;
	login_required: number;
	unknown: number;
	running: number;
}

export interface LogMetrics {
	buffered: number;
	capacity: number;
}

export type EncoderChoice =
	'auto' | 'software' | 'nvenc' | 'vaapi' | 'qsv' | 'videotoolbox' | 'amf' | 'v4l2m2m';

export interface TranscodeMetrics {
	/** The first line of `ffmpeg -version`. */
	ffmpeg: string;
	source: 'embedded' | 'external';
	/** The path of an external build. */
	path: string | null;
	choice: EncoderChoice;
	/** The hardware family in use, when one is. */
	hardware: Exclude<EncoderChoice, 'auto' | 'software'> | null;
	/** The encoder H.264 is made with. */
	h264_encoder: string | null;
	/** Why the choice is not in use, when it is not. */
	shortfall: string | null;
}

export interface LogPage {
	lines: LogLine[];
	next: number | null;
	buffered: number;
	capacity: number;
	oldest_id: number | null;
}

export interface LogLine {
	id: number;
	at: Timestamp;
	level: LogLevel;
	target: string;
	message: string;
	fields: Record<string, string>;
}

export interface Skipped {
	count: number;
}

export type Actor =
	| { kind: 'user'; id: Uuid; username: string; via: Via; ip: Ip }
	| { kind: 'provisioning'; file: string | null };

export interface Target {
	kind: TargetKind;
	id: string;
	name: string | null;
}

/** The fields depend on the entry's action. See the reference's AuditDetails table. */
export type AuditDetails = Record<string, Json>;

// ---- Server-sent events ----

/** The events of `GET /api/events`, by their `event:` name. */
export interface FeedEvents {
	stats: JobStats;
	job: JobEvent;
	bot: BotEvent;
}

/** The events of `GET /api/logs/events`, by their `event:` name. */
export interface LogEvents {
	log: LogLine;
	skipped: Skipped;
}

/** The applications present in a guild, for accounts that manage it on Discord. */
export interface GuildApplication {
	application_id: Uuid;
	name: string;
	guild_name: string;
	present: boolean;
}
