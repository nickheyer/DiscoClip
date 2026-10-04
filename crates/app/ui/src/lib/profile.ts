// The profile editor's form, read from a profile, checked, and written back as the API input

import type {
	AudioCodec,
	AudioContainer,
	BotMessages,
	DedupeMatch,
	DeliveryMode,
	DeliveryOverlay,
	ImageContainer,
	IntakeOverlay,
	MessageOverlay,
	OriginalEmbeds,
	OriginalText,
	OverLimit,
	PermissionMode,
	Placement,
	PlatformDefault,
	Profile,
	ProfileInput,
	ProfileLimits,
	ReplaceAs,
	Requester,
	Snowflake,
	TargetOverride,
	UnderFloor,
	VideoCodec,
	VideoContainer
} from './api/types';

/** What a profile does with the platforms it holds no exception for */
export type PlatformAccess = PlatformDefault | 'presets';

export interface LimitsDraft {
	maxSourceBytes: number | null;
	/** `none` lifts the bound and `limit` sets it to `maxDurationSecs` */
	duration: 'inherit' | 'none' | 'limit';
	maxDurationSecs: number | null;
	maxHeight: number | null;
	maxCaptureSecs: number | null;
}

export interface IntakeDraft {
	/** `everyone` names empty lists and `chosen` the members and roles below */
	posters: 'inherit' | 'everyone' | 'chosen';
	users: Snowflake[];
	roles: Snowflake[];
	botMessages: BotMessages | undefined;
	playlists: boolean | undefined;
	maxEntries: number | null;
	live: boolean | undefined;
}

export interface OutputDraft {
	container: VideoContainer | undefined;
	videoCodec: VideoCodec | undefined;
	audioCodec: AudioCodec | undefined;
	maxHeight: number | null;
	maxFps: number | null;
	audioOverStill: boolean | undefined;
	audio: 'inherit' | 'chosen';
	audioContainers: AudioContainer[];
	images: 'inherit' | 'chosen';
	imageContainers: ImageContainer[];
	files: boolean | undefined;
}

export interface UploadDraft {
	limit: 'inherit' | 'auto' | 'bytes';
	maxBytes: number | null;
}

export interface DeliveryDraft {
	mode: DeliveryMode | undefined;
	/** `auto` or a content view's id */
	view: string | undefined;
	minHeight: number | null;
	/** Bits per second */
	minBitrate: number | null;
	underFloor: UnderFloor | undefined;
	overLimit: OverLimit | undefined;
	linkMaxBytes: number | null;
}

export interface IncludeDraft {
	sourceLink: boolean | undefined;
	title: boolean | undefined;
	platform: boolean | undefined;
	uploader: boolean | undefined;
	requester: Requester | undefined;
	duration: boolean | undefined;
	brand: boolean | undefined;
	earlierPost: boolean | undefined;
}

export interface MessageDraft {
	/** `same` posts where the link was seen and `channel` in the one named below */
	destination: 'inherit' | 'same' | 'channel';
	channel: string;
	placement: Placement | undefined;
	replaceAs: ReplaceAs | undefined;
	originalText: OriginalText | undefined;
	originalEmbeds: OriginalEmbeds | undefined;
	permissions: PermissionMode | undefined;
	include: IncludeDraft;
}

export interface ErrorsDraft {
	debug: boolean | undefined;
}

export interface DedupeDraft {
	enabled: boolean | undefined;
	match: DedupeMatch | undefined;
}

export interface Draft {
	name: string;
	description: string;
	audioLanguage: string | null;
	access: PlatformAccess;
	presets: string[];
	overrides: Record<string, boolean>;
	limits: LimitsDraft;
	intake: IntakeDraft;
	output: OutputDraft;
	upload: UploadDraft;
	delivery: DeliveryDraft;
	message: MessageDraft;
	errors: ErrorsDraft;
	dedupe: DedupeDraft;
}

/** Problems with a draft, keyed by the dotted path of the leaf each is about */
export type Problems = Record<string, string>;

const SNOWFLAKE = /^\d{5,25}$/;

/** A wire `null` inherits like a key left out, so both read as unset */
function named<T>(value: T | null | undefined): T | undefined {
	return value === null ? undefined : value;
}

function number(value: number | null | undefined): number | null {
	return typeof value === 'number' ? value : null;
}

/** The form as it starts for a profile, or empty for a new one */
export function draftOf(profile: Profile | null): Draft {
	const limits = profile?.limits ?? {};
	const intake = profile?.intake ?? {};
	const output = profile?.output ?? {};
	const upload = profile?.upload ?? {};
	const delivery = profile?.delivery ?? {};
	const message = profile?.message ?? {};
	const include = message.include ?? {};
	const platforms = profile?.platforms ?? {};
	const presets = [...(platforms.presets ?? [])];
	const posters = intake.allow_users == null && intake.allow_roles == null;
	const users = intake.allow_users ?? [];
	const roles = intake.allow_roles ?? [];
	return {
		name: profile?.name ?? '',
		description: profile?.description ?? '',
		audioLanguage: profile?.audio_language ?? null,
		access: presets.length > 0 ? 'presets' : (platforms.default ?? 'inherit'),
		presets,
		overrides: { ...(platforms.overrides ?? {}) },
		limits: {
			maxSourceBytes: number(limits.max_source_bytes),
			duration: !('max_duration_secs' in limits)
				? 'inherit'
				: limits.max_duration_secs === null
					? 'none'
					: 'limit',
			maxDurationSecs: number(limits.max_duration_secs),
			maxHeight: number(limits.max_height),
			maxCaptureSecs: number(limits.max_capture_secs)
		},
		intake: {
			posters: posters ? 'inherit' : users.length + roles.length > 0 ? 'chosen' : 'everyone',
			users: [...users],
			roles: [...roles],
			botMessages: named(intake.bot_messages),
			playlists: named(intake.playlists?.enabled),
			maxEntries: number(intake.playlists?.max_entries),
			live: named(intake.live)
		},
		output: {
			container: named(output.container),
			videoCodec: named(output.video_codec),
			audioCodec: named(output.audio_codec),
			maxHeight: number(output.max_height),
			maxFps: number(output.max_fps),
			audioOverStill: named(output.audio_over_still),
			audio: output.audio_containers == null ? 'inherit' : 'chosen',
			audioContainers: [...(output.audio_containers ?? [])],
			images: output.image_containers == null ? 'inherit' : 'chosen',
			imageContainers: [...(output.image_containers ?? [])],
			files: named(output.files)
		},
		upload: {
			limit: upload.max_bytes == null ? 'inherit' : upload.max_bytes === 'auto' ? 'auto' : 'bytes',
			maxBytes: number(upload.max_bytes === 'auto' ? null : upload.max_bytes)
		},
		delivery: {
			mode: named(delivery.mode),
			view: named(delivery.view),
			minHeight: number(delivery.floor?.min_height),
			minBitrate: number(delivery.floor?.min_bitrate),
			underFloor: named(delivery.under_floor),
			overLimit: named(delivery.over_limit),
			linkMaxBytes: number(delivery.link_max_bytes)
		},
		message: {
			destination: !('destination' in message)
				? 'inherit'
				: message.destination === null
					? 'same'
					: 'channel',
			channel: message.destination ?? '',
			placement: named(message.placement),
			replaceAs: named(message.replace_as),
			originalText: named(message.original_text),
			originalEmbeds: named(message.original_embeds),
			permissions: named(message.permissions),
			include: {
				sourceLink: named(include.source_link),
				title: named(include.title),
				platform: named(include.platform),
				uploader: named(include.uploader),
				requester: named(include.requester),
				duration: named(include.duration),
				brand: named(include.brand),
				earlierPost: named(include.earlier_post)
			}
		},
		errors: { debug: named(profile?.errors?.debug) },
		dedupe: {
			enabled: named(profile?.dedupe?.enabled),
			match: named(profile?.dedupe?.match)
		}
	};
}

/** The input a draft stands for with every problem found, the built-in profile having to name every leaf */
export function inputOf(
	draft: Draft,
	builtin: boolean
): { input: ProfileInput; problems: Problems } {
	const problems: Problems = {};
	const fail = (path: string, message: string) => {
		problems[path] = message;
	};
	/** A number above zero, or inherited when not named */
	const positive = (path: string, value: number | null): number | undefined => {
		if (value === null) {
			if (builtin) fail(path, 'Required');
			return undefined;
		}
		if (!(value > 0)) fail(path, 'Above zero');
		return value;
	};
	/** A choice, which the built-in profile has to make */
	const chosen = <T>(path: string, value: T | undefined): T | undefined => {
		if (value === undefined && builtin) fail(path, 'Required');
		return value;
	};
	const ids = (path: string, list: Snowflake[]): Snowflake[] => {
		const trimmed = list.map((id) => id.trim()).filter(Boolean);
		if (trimmed.some((id) => !SNOWFLAKE.test(id))) fail(path, 'Discord ids only');
		return trimmed;
	};

	const name = draft.name.trim();
	if (!name) fail('name', 'Required');
	if (draft.access === 'presets' && draft.presets.length === 0)
		fail('platforms.presets', 'Required');

	const limits: ProfileLimits = {};
	const maxSourceBytes = positive('limits.max_source_bytes', draft.limits.maxSourceBytes);
	if (maxSourceBytes !== undefined) limits.max_source_bytes = Math.round(maxSourceBytes);
	switch (draft.limits.duration) {
		case 'inherit':
			if (builtin) fail('limits.max_duration_secs', 'Required');
			break;
		case 'none':
			limits.max_duration_secs = null;
			break;
		case 'limit':
			if (draft.limits.maxDurationSecs === null) fail('limits.max_duration_secs', 'Required');
			else limits.max_duration_secs = Math.round(draft.limits.maxDurationSecs);
	}
	const maxHeight = positive('limits.max_height', draft.limits.maxHeight);
	if (maxHeight !== undefined) limits.max_height = maxHeight;
	const maxCapture = positive('limits.max_capture_secs', draft.limits.maxCaptureSecs);
	if (maxCapture !== undefined) limits.max_capture_secs = Math.round(maxCapture);

	const intake: IntakeOverlay = {};
	switch (draft.intake.posters) {
		case 'inherit':
			if (builtin) fail('intake.allow_users', 'Required');
			break;
		case 'everyone':
			intake.allow_users = [];
			intake.allow_roles = [];
			break;
		case 'chosen':
			intake.allow_users = ids('intake.allow_users', draft.intake.users);
			intake.allow_roles = ids('intake.allow_roles', draft.intake.roles);
	}
	const botMessages = chosen('intake.bot_messages', draft.intake.botMessages);
	if (botMessages !== undefined) intake.bot_messages = botMessages;
	const playlists = chosen('intake.playlists.enabled', draft.intake.playlists);
	const maxEntries = positive('intake.playlists.max_entries', draft.intake.maxEntries);
	if (playlists !== undefined || maxEntries !== undefined) {
		intake.playlists = {};
		if (playlists !== undefined) intake.playlists.enabled = playlists;
		if (maxEntries !== undefined) intake.playlists.max_entries = Math.round(maxEntries);
	}
	const live = chosen('intake.live', draft.intake.live);
	if (live !== undefined) intake.live = live;

	const output: TargetOverride = {};
	const container = chosen('output.container', draft.output.container);
	if (container !== undefined) output.container = container;
	const videoCodec = chosen('output.video_codec', draft.output.videoCodec);
	if (videoCodec !== undefined) output.video_codec = videoCodec;
	const audioCodec = chosen('output.audio_codec', draft.output.audioCodec);
	if (audioCodec !== undefined) output.audio_codec = audioCodec;
	if (draft.output.maxHeight !== null) {
		if (!(draft.output.maxHeight > 0)) fail('output.max_height', 'Above zero');
		output.max_height = draft.output.maxHeight;
	}
	if (draft.output.maxFps !== null) {
		if (!(draft.output.maxFps > 0)) fail('output.max_fps', 'Above zero');
		output.max_fps = Math.round(draft.output.maxFps);
	}
	const audioOverStill = chosen('output.audio_over_still', draft.output.audioOverStill);
	if (audioOverStill !== undefined) output.audio_over_still = audioOverStill;
	if (draft.output.audio === 'chosen') output.audio_containers = [...draft.output.audioContainers];
	else if (builtin) fail('output.audio_containers', 'Required');
	if (draft.output.images === 'chosen') output.image_containers = [...draft.output.imageContainers];
	else if (builtin) fail('output.image_containers', 'Required');
	const files = chosen('output.files', draft.output.files);
	if (files !== undefined) output.files = files;

	const upload: ProfileInput['upload'] = {};
	switch (draft.upload.limit) {
		case 'inherit':
			if (builtin) fail('upload.max_bytes', 'Required');
			break;
		case 'auto':
			upload.max_bytes = 'auto';
			break;
		case 'bytes': {
			if (draft.upload.maxBytes === null) fail('upload.max_bytes', 'Required');
			else if (!(draft.upload.maxBytes > 0)) fail('upload.max_bytes', 'Above zero');
			else upload.max_bytes = Math.round(draft.upload.maxBytes);
		}
	}

	const delivery: DeliveryOverlay = {};
	const mode = chosen('delivery.mode', draft.delivery.mode);
	if (mode !== undefined) delivery.mode = mode;
	const view = chosen('delivery.view', draft.delivery.view);
	if (view !== undefined) delivery.view = view;
	const minHeight = positive('delivery.floor.min_height', draft.delivery.minHeight);
	const minBitrate = positive('delivery.floor.min_bitrate', draft.delivery.minBitrate);
	if (minHeight !== undefined || minBitrate !== undefined) {
		delivery.floor = {};
		if (minHeight !== undefined) delivery.floor.min_height = minHeight;
		if (minBitrate !== undefined) delivery.floor.min_bitrate = Math.round(minBitrate);
	}
	const underFloor = chosen('delivery.under_floor', draft.delivery.underFloor);
	if (underFloor !== undefined) delivery.under_floor = underFloor;
	const overLimit = chosen('delivery.over_limit', draft.delivery.overLimit);
	if (overLimit !== undefined) delivery.over_limit = overLimit;
	const linkMax = positive('delivery.link_max_bytes', draft.delivery.linkMaxBytes);
	if (linkMax !== undefined) delivery.link_max_bytes = Math.round(linkMax);

	const message: MessageOverlay = {};
	switch (draft.message.destination) {
		case 'inherit':
			if (builtin) fail('message.destination', 'Required');
			break;
		case 'same':
			message.destination = null;
			break;
		case 'channel': {
			const channel = draft.message.channel.trim();
			if (!SNOWFLAKE.test(channel)) fail('message.destination', 'A Discord channel id');
			message.destination = channel;
		}
	}
	const placement = chosen('message.placement', draft.message.placement);
	if (placement !== undefined) message.placement = placement;
	const replaceAs = chosen('message.replace_as', draft.message.replaceAs);
	if (replaceAs !== undefined) message.replace_as = replaceAs;
	const originalText = chosen('message.original_text', draft.message.originalText);
	if (originalText !== undefined) message.original_text = originalText;
	const originalEmbeds = chosen('message.original_embeds', draft.message.originalEmbeds);
	if (originalEmbeds !== undefined) message.original_embeds = originalEmbeds;
	const permissions = chosen('message.permissions', draft.message.permissions);
	if (permissions !== undefined) message.permissions = permissions;
	const inc = draft.message.include;
	const include: NonNullable<MessageOverlay['include']> = {};
	const sourceLink = chosen('message.include.source_link', inc.sourceLink);
	if (sourceLink !== undefined) include.source_link = sourceLink;
	const title = chosen('message.include.title', inc.title);
	if (title !== undefined) include.title = title;
	const platform = chosen('message.include.platform', inc.platform);
	if (platform !== undefined) include.platform = platform;
	const uploader = chosen('message.include.uploader', inc.uploader);
	if (uploader !== undefined) include.uploader = uploader;
	const requester = chosen('message.include.requester', inc.requester);
	if (requester !== undefined) include.requester = requester;
	const duration = chosen('message.include.duration', inc.duration);
	if (duration !== undefined) include.duration = duration;
	const brand = chosen('message.include.brand', inc.brand);
	if (brand !== undefined) include.brand = brand;
	const earlierPost = chosen('message.include.earlier_post', inc.earlierPost);
	if (earlierPost !== undefined) include.earlier_post = earlierPost;
	if (Object.keys(include).length > 0) message.include = include;

	const errors: ProfileInput['errors'] = {};
	const debug = chosen('errors.debug', draft.errors.debug);
	if (debug !== undefined) errors.debug = debug;

	const dedupe: ProfileInput['dedupe'] = {};
	const enabled = chosen('dedupe.enabled', draft.dedupe.enabled);
	if (enabled !== undefined) dedupe.enabled = enabled;
	const match = chosen('dedupe.match', draft.dedupe.match);
	if (match !== undefined) dedupe.match = match;

	return {
		input: {
			name,
			description: draft.description.trim(),
			audio_language: draft.audioLanguage,
			platforms: {
				default: draft.access === 'presets' ? 'inherit' : draft.access,
				presets: draft.access === 'presets' ? [...draft.presets] : [],
				overrides: { ...draft.overrides }
			},
			limits,
			intake,
			output,
			upload,
			delivery,
			message,
			errors,
			dedupe
		},
		problems
	};
}

/** Whether anything under the value is named, a list counting even when empty */
function anyNamed(value: unknown): boolean {
	if (value === null || value === undefined) return false;
	if (Array.isArray(value)) return true;
	if (typeof value === 'object') return Object.values(value).some(anyNamed);
	return true;
}

const SECTIONS: [
	keyof Pick<
		Profile,
		'limits' | 'intake' | 'output' | 'upload' | 'delivery' | 'message' | 'errors' | 'dedupe'
	>,
	string
][] = [
	['limits', 'Limits'],
	['intake', 'Intake'],
	['output', 'Output'],
	['upload', 'Upload'],
	['delivery', 'Delivery'],
	['message', 'Message'],
	['errors', 'Errors'],
	['dedupe', 'Dedupe']
];

/** The sections a profile names anything in, a lifted duration and a same-channel destination counting */
export function namedSections(profile: Profile): string[] {
	return SECTIONS.filter(([key]) => {
		const section = profile[key] ?? {};
		if (key === 'limits' && 'max_duration_secs' in section) return true;
		if (key === 'message' && 'destination' in section) return true;
		return anyNamed(section);
	}).map(([, label]) => label);
}
