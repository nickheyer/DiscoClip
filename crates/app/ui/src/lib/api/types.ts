// Wire types, one for one with API.md.

export type Role = 'admin' | 'operator' | 'viewer';

export type Permission =
	| 'manage_users'
	| 'manage_applications'
	| 'manage_watch_rules'
	| 'manage_bots'
	| 'view_audit_log'
	| 'manage_jobs';

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
	'view_audit_log',
	'manage_jobs'
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

export type VariantKind = 'file' | 'hls' | 'dash' | 'ism' | 'rtmp' | 'rtsp' | 'whep' | 'browser';

export type Codec = string | { other: string };

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

export interface Artifacts {
	resolved: Resolved | null;
	source: LocalFile | null;
	output: LocalFile | null;
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
