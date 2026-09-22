// Wire types, one for one with API.md.

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

export type TargetKind = 'setting' | 'application' | 'rule' | 'platform' | 'profile' | 'frontend';

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
	| 'frontend.sessions.revoke';

export const ROLES: Role[] = ['admin', 'operator', 'viewer'];

export const PERMISSIONS: Permission[] = [
	'manage_users',
	'manage_applications',
	'manage_watch_rules',
	'manage_bots',
	'view_audit_log',
	'manage_jobs',
	'manage_settings',
	'view_logs'
];

export type StatusKind = 'queued' | 'running' | 'done' | 'failed' | 'cancelled';

export const STATUS_KINDS: StatusKind[] = ['queued', 'running', 'done', 'failed', 'cancelled'];

export type Stage = 'resolve' | 'download' | 'transcode' | 'publish' | 'archive';

export const STAGES: Stage[] = ['resolve', 'download', 'transcode', 'publish', 'archive'];

export type JobOrder = 'newest' | 'oldest';

export const COMMAND_MODES: CommandMode[] = ['off', 'global', 'guilds'];

export const BOT_STATES: BotStateName[] = [
	'disabled',
	'stopped',
	'starting',
	'connected',
	'retrying',
	'failed'
];

export const TARGET_KINDS: TargetKind[] = ['setting', 'application', 'rule', 'platform'];

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
	'rule.delete',
	'session.import',
	'session.clear'
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

/** Watch channel, destination and allowed submitters. Profiles define platform access and limits. */
export interface RuleInput {
	channel_id: string;
	post_to?: string | null;
	allow_users?: string[];
	allow_roles?: string[];
	enabled?: boolean;
}

export interface JobQuery {
	source?: string;
	status?: StatusKind;
	resolver?: string;
	parent?: string;
	top_level?: boolean;
	q?: string;
	before?: string;
	after?: string;
	limit?: number;
	offset?: number;
	order?: JobOrder;
}

export interface SubmitRequest {
	url: string;
	limits?: Partial<RequestLimits>;
	options?: Partial<RequestOptions>;
}

export interface Submitted {
	id: string;
}

export type BulkAction = 'retry' | 'cancel' | 'delete';

export interface BulkRequest {
	action: BulkAction;
	ids: string[];
}

export interface BulkOutcome {
	id: string;
	ok: boolean;
	error: string | null;
	job: string | null;
}

export interface BulkResponse {
	action: BulkAction;
	results: BulkOutcome[];
	succeeded: number;
	failed: number;
}

export type Artifact = 'output' | 'source' | 'subtitle';

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

export interface RoleView {
	role: Role;
	description: string;
	permissions: Permission[];
	accounts: User[];
}

export interface AccountSessionView extends SessionView {
	user_id: string;
	username: string;
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
	/** Where Discord sends browsers back to after a login. Registered at Discord. */
	login_callback_url: string;
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

export type BotEvent = BotStatus & {
	application: string;
	/** The application was removed, and this is the last word on its bot. */
	removed?: true;
};

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

export type ChannelKind =
	| 'text'
	| 'announcement'
	| 'voice'
	| 'stage'
	| 'category'
	| 'forum'
	| 'media'
	| 'thread'
	| 'other';

export const CHANNEL_KINDS: ChannelKind[] = [
	'text',
	'announcement',
	'voice',
	'stage',
	'category',
	'forum',
	'media',
	'thread',
	'other'
];

export interface GuildChannel {
	id: string;
	name: string;
	kind: ChannelKind;
	parent_id: string | null;
	position: number;
	/** The rule watching the channel, when one does. */
	rule: string | null;
}

export interface GuildRole {
	id: string;
	name: string;
	color: number;
	position: number;
	managed: boolean;
}

export interface GuildMember {
	id: string;
	username: string;
	display_name: string | null;
	nick: string | null;
	avatar: string | null;
	bot: boolean;
}

// Profiles

/** What a profile says about the platforms it does not name. */
export type PlatformDefault = 'inherit' | 'enabled' | 'disabled';

export const PLATFORM_DEFAULTS: PlatformDefault[] = ['inherit', 'enabled', 'disabled'];

/** What kind of place a platform is. A profile can turn platforms on by kind. */
export type PlatformTag =
	| 'basic'
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

export const PLATFORM_TAGS: PlatformTag[] = [
	'basic',
	'nsfw',
	'news',
	'social',
	'video',
	'music',
	'podcasts',
	'live',
	'files',
	'images',
	'players'
];

/** A preset: a named set of platforms, from the tags the platforms carry. */
export interface Preset {
	id: string;
	label: string;
	description: string;
	/** The platforms in it, by resolver id. */
	platforms: string[];
}

/** Presets replace the default with their combined platform list. Explicit overrides take priority. */
export interface PlatformToggles {
	default: PlatformDefault;
	presets: string[];
	overrides: Record<string, boolean>;
}

/** Media limits. Null inherits the parent value. Server limits cap every profile. */
export interface ProfileLimits {
	max_source_bytes: number | null;
	max_duration_secs: number | null;
	max_height: number | null;
}

export interface ProfileInput {
	name: string;
	description: string;
	platforms: PlatformToggles;
	limits: ProfileLimits;
}

export interface Profile extends ProfileInput {
	id: string;
	/** Ships with the server and cannot be removed. */
	builtin: boolean;
	created_at: string;
	updated_at: string;
}

/** Where a profile applies. */
export type Scope =
	| { kind: 'global' }
	| { kind: 'guild'; guild_id: string }
	| { kind: 'channel'; guild_id: string; channel_id: string }
	| { kind: 'user'; guild_id: string; user_id: string };

export interface Assignment {
	scope: Scope;
	profile_id: string;
	updated_at: string;
}

/** Effective settings after applying assignments. */
export interface EffectiveProfile {
	platforms: Record<string, boolean>;
	/** The limits assigned, each from the narrowest profile that names it. */
	limits: RequestLimits;
	/** The assignments applied to get there, widest first. */
	applied: Assignment[];
}

export interface EffectiveView extends EffectiveProfile {
	/** The resolver ids turned off, as the engine is told. */
	disabled: string[];
}

export interface Rule {
	id: string;
	application_id: string;
	guild_id: string;
	channel_id: string;
	post_to: string | null;
	allow_users: string[];
	allow_roles: string[];
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

// Media sites

/** Which jobs a media site shows. Both empty means every job. */
export interface ContentScope {
	guilds: string[];
	channels: string[];
}

export type SecretKind = 'pin' | 'password' | 'token';

export const SECRET_KINDS: SecretKind[] = ['pin', 'password', 'token'];

export interface FrontAccessInput {
	open: boolean;
	secret_kind: SecretKind | null;
	accounts: boolean;
	providers: string[];
	discord_members: boolean;
	discord_users: string[];
}

export interface LinkPolicy {
	enabled: boolean;
	min_height: number;
	min_bitrate: number;
	max_bytes: number;
	signed_link_days: number;
}

export interface FrontendInput {
	name: string;
	slug: string;
	description: string;
	enabled: boolean;
	profile_id: string;
	scope: ContentScope;
	access: FrontAccessInput;
	downloads: boolean;
	links: LinkPolicy;
}

export interface Frontend extends FrontendInput {
	id: string;
	has_secret: boolean;
	created_at: string;
	updated_at: string;
}

export interface FrontendUser {
	id: string;
	frontend_id: string;
	username: string;
	created_at: string;
}

export interface ViewerSession {
	id: string;
	frontend_id: string;
	subject: string;
	display: string;
	created_at: string;
	last_seen_at: string;
	expires_at: string;
	ip: string | null;
	user_agent: string | null;
}

export interface Viewer {
	frontend_id: string;
	subject: string;
	display: string;
}

export interface FrontProvider {
	id: string;
	name: string;
}

export interface FrontAccess {
	open: boolean;
	secret: SecretKind | null;
	accounts: boolean;
	providers: FrontProvider[];
	discord_members: boolean;
}

/** A media site as its visitors see it. */
export interface FrontInfo {
	slug: string;
	name: string;
	description: string;
	downloads: boolean;
	access: FrontAccess;
	platforms: string[];
	viewer: Viewer | null;
}

export interface FrontLogin {
	secret?: string;
	username?: string;
	password?: string;
}

export interface FrontJob {
	id: string;
	title: string | null;
	media: MediaKind;
	resolver: string;
	uploader: string | null;
	webpage_url: string | null;
	thumbnail: string | null;
	duration_secs: number | null;
	live: boolean;
	size: number;
	width: number | null;
	height: number | null;
	content_type: string;
	published_at: string;
	media_url: string;
	download_url: string | null;
}

export interface FrontPage {
	jobs: FrontJob[];
	next: string | null;
}

export interface FrontJobQuery {
	q?: string;
	media?: MediaKind;
	resolver?: string;
	before?: string;
	limit?: number;
}

/** Why a provider login into a media site came back to its login page. */
export type FrontCallbackError =
	| 'state'
	| 'denied'
	| 'provider'
	| 'exchange'
	| 'identity'
	| 'frontend'
	| 'not_listed'
	| 'not_member'
	| 'guilds';

// Jobs

/** A `std::time::Duration` on the wire. */
export interface DurationWire {
	secs: number;
	nanos: number;
}

export function durationSeconds(value: DurationWire | null | undefined): number | null {
	if (!value) return null;
	return value.secs + value.nanos / 1e9;
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
	url: string | null;
}

export interface RequestLimits {
	max_source_bytes: number | null;
	max_duration_secs: number | null;
	max_height: number | null;
}

export interface ClipRange {
	start: DurationWire;
	end: DurationWire | null;
}

export type SubtitleMode = 'keep' | 'burn' | 'skip';

export interface RequestOptions {
	clip: ClipRange | null;
	subtitles: SubtitleMode;
	subtitle_language: string | null;
}

export interface JobRequest {
	origin: Origin;
	url: string;
	destination: string | null;
	limits: RequestLimits;
	options: RequestOptions;
	parent: string | null;
	retry_of: string | null;
	submitted_by: string | null;
}

export interface JobSummary {
	id: string;
	url: string;
	status: JobStatus;
	source: string;
	origin: Origin;
	destination: string | null;
	submitted_by: string | null;
	parent: string | null;
	retry_of: string | null;
	title: string | null;
	resolver: string | null;
	/** What the media is: what the probe found, else what the resolver said, else video. */
	media: MediaKind;
	uploader: string | null;
	webpage_url: string | null;
	thumbnail: string | null;
	duration_secs: number | null;
	live: boolean;
	output_bytes: number | null;
	published_url: string | null;
	published_reference: string | null;
	children: number;
	archived_files: number;
	created_at: string;
	updated_at: string;
	started_at: string | null;
	finished_at: string | null;
}

export interface JobPage {
	jobs: JobSummary[];
	total: number;
	limit: number;
	offset: number;
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
	last_done_at: string | null;
	last_failed_at: string | null;
}

export interface JobStats {
	counts: Stats;
	last_24h: Stats;
	utilisation: Utilisation;
	queue_depth: number;
	active: string[];
	resolvers: ResolverStats[];
	at: string;
}

export interface Progress {
	done: number;
	total: number | null;
}

export interface LogEntry {
	at: string;
	stage: Stage | null;
	message: string;
}

export type JobEvent = { job: string; at: string; job_summary: JobSummary | null } & (
	| { kind: 'submitted'; request: JobRequest }
	| { kind: 'status'; status: JobStatus }
	| { kind: 'progress'; stage: Stage; progress: Progress }
	| { kind: 'log'; entry: LogEntry }
	| { kind: 'children'; ids: string[] }
	| { kind: 'deleted' }
);

export type VariantKind = 'file' | 'hls' | 'dash' | 'ism' | 'rtmp' | 'rtsp' | 'rtp' | 'whep' | 'browser';

/** What a piece of media is: a moving picture, sound alone, a still, or any other file. */
export type MediaKind = 'video' | 'audio' | 'image' | 'file';

export const MEDIA_KINDS: MediaKind[] = ['video', 'audio', 'image', 'file'];

export const MEDIA_LABELS: Record<MediaKind, string> = {
	video: 'Video',
	audio: 'Audio',
	image: 'Image',
	file: 'File'
};

export type Codec = string | { other: string };

/** How a file's bytes are decrypted as they download, when the host stores them encrypted. */
export type Cipher = { scheme: 'aes128_ctr'; key: number[]; nonce: number[] };

export interface Variant {
	url: string;
	kind: VariantKind;
	audio_url: string | null;
	container: Codec | null;
	video: Codec | null;
	audio: Codec | null;
	width: number | null;
	height: number | null;
	fps: number | null;
	bitrate: number | null;
	size: number | null;
	duration: DurationWire | null;
	headers: [string, string][];
	format_id: string | null;
	label: string | null;
	language: string | null;
	codecs: string | null;
	video_only: boolean;
	audio_only: boolean;
	live: boolean;
	drm: string | null;
	cipher?: Cipher | null;
}

export interface SubtitleTrack {
	url: string;
	language: string;
	name: string | null;
	format: string;
	auto: boolean;
	headers: [string, string][];
}

export interface Resolved {
	resolver: string;
	/** What the link is. Decides which variant is picked and how it is shrunk and shown. */
	media: MediaKind;
	id: string | null;
	title: string | null;
	description: string | null;
	uploader: string | null;
	uploader_url: string | null;
	uploaded_at: string | null;
	duration: DurationWire | null;
	thumbnail: string | null;
	webpage_url: string | null;
	live: boolean;
	age_limit: number | null;
	clip: ClipRange | null;
	subtitles: SubtitleTrack[];
	variants: Variant[];
}

export interface VideoTrack {
	codec: Codec;
	width: number;
	height: number;
	fps: number | null;
	bitrate: number | null;
}

export interface AudioTrack {
	codec: Codec;
	channels: number;
	sample_rate: number;
	bitrate: number | null;
}

export interface MediaInfo {
	container: Codec;
	/** What the probe found the file to be. A still has its picture in `video` with no fps. */
	kind: MediaKind;
	duration: DurationWire | null;
	video: VideoTrack | null;
	audio: AudioTrack | null;
}

export interface LocalFile {
	path: string;
	size: number;
	info: MediaInfo | null;
}

export interface LocalSubtitle {
	language: string;
	name: string | null;
	path: string;
	format: string;
}

export interface Published {
	reference: string;
	url: string | null;
	at: string;
}

export interface ArchiveEntry {
	files: string[];
	bytes: number;
	at: string;
}

export interface StageTiming {
	stage: Stage;
	started_at: string;
	ended_at: string | null;
}

/** How the output reached the destination: the file itself, or a link to its page. */
export type Delivery = 'upload' | 'link';

export interface Artifacts {
	resolved: Resolved | null;
	source: LocalFile | null;
	output: LocalFile | null;
	/** Whether the output was handed over or linked to. */
	delivery: Delivery;
	/** Why a media site's page was posted instead of the file, when it was. */
	link_reason: string | null;
	published: Published | null;
	archived: ArchiveEntry | null;
	subtitles: LocalSubtitle[];
	children: string[];
	timings: StageTiming[];
}

export interface Job {
	id: string;
	request: JobRequest;
	status: JobStatus;
	artifacts: Artifacts;
	log: LogEntry[];
	created_at: string;
	updated_at: string;
	started_at: string | null;
	finished_at: string | null;
}

/** The name of a codec or container as the wire carries it. */
export function codecName(codec: Codec | null | undefined): string | null {
	if (codec == null) return null;
	return typeof codec === 'string' ? codec : codec.other;
}

// Settings

export type SettingSource = 'provisioning' | 'app';

export type SettingsFormat = 'toml' | 'yaml' | 'json';

export const SETTINGS_FORMATS: SettingsFormat[] = ['toml', 'yaml', 'json'];

/** A JSON value as the settings hold it. */
// Platforms

export type SessionSupport = 'none' | 'optional' | 'required';

export type FixtureStatus = 'pass' | 'fail' | 'login_required' | 'never';

/** What a fixture link resolved to: media of a kind with playable variants, or a playlist. */
export type Found =
	| { kind: 'media'; media: MediaKind; variants: number }
	| { kind: 'playlist'; entries: number };

export interface FixtureResult {
	url: string;
	status: FixtureStatus;
	run_at: string | null;
	last_pass_at: string | null;
	error: string | null;
	title: string | null;
	found: Found | null;
	duration_ms: number | null;
}

export type SessionState = 'unsupported' | 'logged_out' | 'logged_in';

export type SessionCheckResult = { at: string } & (
	| { state: 'unsupported' }
	| { state: 'logged_out' }
	| { state: 'logged_in'; account: string }
);

export interface PlatformCoverage {
	id: string;
	name: string;
	hosts: string[];
	features: string[];
	formats: string[];
	/** What its links resolve to: videos, audio, images or other files. */
	media: MediaKind[];
	/** What kind of place it is: the presets it belongs to. */
	tags: PlatformTag[];
	session: SessionSupport;
	cookies: number;
	fixtures: FixtureResult[];
	last_run_at: string | null;
	last_pass_at: string | null;
	last_fail_at: string | null;
	passed: number;
	failed: number;
	/** Fixtures of the last run that resolve only with a login the platform's jar lacks. */
	login_required: number;
	running: boolean;
	cookies_updated_at: string | null;
	session_check: SessionCheckResult | null;
}

export type CookieFormat = 'netscape' | 'header';

export interface CookiesImport {
	format: CookieFormat;
	text: string;
	domain?: string;
}

export interface SessionOutcome extends PlatformCoverage {
	check_error: string | null;
}

export interface CheckStarted {
	platforms: string[];
}

// Health, metrics and the log

export type HealthStatus = 'ok' | 'warn' | 'fail';

export interface HealthCheck {
	name: string;
	label: string;
	status: HealthStatus;
	detail: string;
}

export interface Health {
	status: HealthStatus;
	version: string;
	started_at: string;
	uptime_secs: number;
	at: string;
	checks: HealthCheck[];
}

export interface ProcessMetrics {
	pid: number;
	rss_bytes: number;
	virtual_bytes: number;
	cpu_percent: number;
	run_time_secs: number;
}

export interface DiskMetrics {
	mount: string;
	total_bytes: number;
	available_bytes: number;
	holds: string[];
}

export interface SystemMetrics {
	total_memory_bytes: number;
	available_memory_bytes: number;
	load_average: [number, number, number];
	cpus: number;
	disks: DiskMetrics[];
}

export interface RequestCount {
	host: string;
	status: number;
	count: number;
}

export interface HttpMetrics {
	requests: RequestCount[];
	retries: number;
	rate_limit_waits: number;
	bytes_received: number;
}

export interface BotMetrics {
	applications: number;
	by_state: Record<string, number>;
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
	with_fixtures: number;
	passing: number;
	failing: number;
	never: number;
	running: number;
}

export interface LogMetrics {
	buffered: number;
	capacity: number;
}

export interface Metrics {
	at: string;
	version: string;
	started_at: string;
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
}

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

export const LOG_LEVELS: LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

export interface LogLine {
	id: number;
	at: string;
	level: LogLevel;
	target: string;
	message: string;
	fields: Record<string, string>;
}

export interface LogQuery {
	level?: LogLevel;
	target?: string;
	q?: string;
	before?: number;
	limit?: number;
}

export interface LogPage {
	lines: LogLine[];
	next: number | null;
	buffered: number;
	capacity: number;
	oldest_id: number | null;
}

export interface Skipped {
	count: number;
}

export type SettingValue =
	| string
	| number
	| boolean
	| null
	| SettingValue[]
	| { [key: string]: SettingValue };

export interface SettingEntry {
	key: string;
	source: SettingSource;
	updated_at: string;
}

export interface SettingsView {
	settings: Record<string, SettingValue>;
	defaults: Record<string, SettingValue>;
	entries: SettingEntry[];
	secrets: string[];
	data_dir: string;
	provisioning_file: string | null;
}

export interface SettingsChange {
	set?: Record<string, SettingValue>;
	reset?: string[];
}

export interface SettingSetRequest {
	value: SettingValue;
}

export interface SettingsImportRequest {
	format: SettingsFormat;
	text: string;
}
