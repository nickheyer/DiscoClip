// Formatting shared by every page.

import type { Duration, MediaKind, RequestLimits, Snowflake, Stage, Timestamp } from './api/types';

/** The word shown where a value is not set. */
export const EMPTY = 'None';

/** The word shown where a time has not happened. */
export const NEVER = 'Never';

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];

/** `1.5 MB`, `12 KB`, `0 B`. Binary multiples. */
export function bytes(n: number | null | undefined): string {
	if (n === null || n === undefined || !Number.isFinite(n)) return '';
	let value = Math.max(0, n);
	let unit = 0;
	while (value >= 1024 && unit < UNITS.length - 1) {
		value /= 1024;
		unit += 1;
	}
	const digits = unit === 0 ? 0 : value < 10 ? 1 : 0;
	return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

/** `1:02:03` or `4:05` from seconds. */
export function clock(secs: number | null | undefined): string {
	if (secs === null || secs === undefined || !Number.isFinite(secs)) return '';
	const total = Math.max(0, Math.round(secs));
	const h = Math.floor(total / 3600);
	const m = Math.floor((total % 3600) / 60);
	const s = total % 60;
	const mm = h > 0 ? String(m).padStart(2, '0') : String(m);
	return `${h > 0 ? `${h}:` : ''}${mm}:${String(s).padStart(2, '0')}`;
}

/** The clock form of a wire duration. */
export function duration(d: Duration | null | undefined): string {
	if (!d) return '';
	return clock(d.secs + d.nanos / 1e9);
}

/** Seconds of a wire duration. */
export function durationSecs(d: Duration | null | undefined): number | null {
	if (!d) return null;
	return d.secs + d.nanos / 1e9;
}

/** A wire duration from seconds. */
export function toDuration(secs: number): Duration {
	const whole = Math.floor(secs);
	return { secs: whole, nanos: Math.round((secs - whole) * 1e9) };
}

const RELATIVE_UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
	['year', 365 * 24 * 3600],
	['month', 30 * 24 * 3600],
	['week', 7 * 24 * 3600],
	['day', 24 * 3600],
	['hour', 3600],
	['minute', 60]
];

const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

/** `3 minutes ago`, `in 2 hours`, `just now`. */
export function relative(
	at: Timestamp | Date | null | undefined,
	now: number = Date.now()
): string {
	if (!at) return '';
	const time = typeof at === 'string' ? Date.parse(at) : at.getTime();
	if (Number.isNaN(time)) return '';
	const delta = (time - now) / 1000;
	const magnitude = Math.abs(delta);
	if (magnitude < 45) return 'just now';
	for (const [unit, seconds] of RELATIVE_UNITS) {
		if (magnitude >= seconds) {
			return relativeFormat.format(Math.round(delta / seconds), unit);
		}
	}
	return relativeFormat.format(Math.round(delta), 'second');
}

const absoluteFormat = new Intl.DateTimeFormat(undefined, {
	dateStyle: 'medium',
	timeStyle: 'medium'
});

/** The full local date and time, for titles beside a relative time. */
export function absolute(at: Timestamp | Date | null | undefined): string {
	if (!at) return '';
	const date = typeof at === 'string' ? new Date(at) : at;
	if (Number.isNaN(date.getTime())) return '';
	return absoluteFormat.format(date);
}

const dateOnly = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' });

/** The local date without the time. */
export function date(at: Timestamp | Date | null | undefined): string {
	if (!at) return '';
	const value = typeof at === 'string' ? new Date(at) : at;
	if (Number.isNaN(value.getTime())) return '';
	return dateOnly.format(value);
}

/** `42%` from a ratio in 0..1. */
export function percent(ratio: number | null | undefined, digits = 0): string {
	if (ratio === null || ratio === undefined || !Number.isFinite(ratio)) return '';
	return `${(ratio * 100).toFixed(digits)}%`;
}

const numberFormat = new Intl.NumberFormat();

/** A count with the locale's grouping. */
export function number(n: number | null | undefined): string {
	if (n === null || n === undefined || !Number.isFinite(n)) return '';
	return numberFormat.format(n);
}

/** Seconds as `2h 5m`, `45s`, `3d 2h`. */
export function span(secs: number | null | undefined): string {
	if (secs === null || secs === undefined || !Number.isFinite(secs)) return '';
	const total = Math.max(0, Math.round(secs));
	const d = Math.floor(total / 86400);
	const h = Math.floor((total % 86400) / 3600);
	const m = Math.floor((total % 3600) / 60);
	const s = total % 60;
	if (d > 0) return `${d}d ${h}h`;
	if (h > 0) return `${h}h ${m}m`;
	if (m > 0) return `${m}m ${s}s`;
	return `${s}s`;
}

const EVERY_UNITS: [string, number][] = [
	['day', 86400],
	['hour', 3600],
	['minute', 60],
	['second', 1]
];

/** `Every day`, `Every 6 hours`, `Every 1 hour 30 minutes`: how often something runs. */
export function every(secs: number | null | undefined): string {
	if (secs === null || secs === undefined || !Number.isFinite(secs)) return '';
	let rest = Math.max(0, Math.round(secs));
	const parts: [number, string][] = [];
	for (const [unit, seconds] of EVERY_UNITS) {
		const count = Math.floor(rest / seconds);
		if (count === 0) continue;
		rest -= count * seconds;
		parts.push([count, unit]);
	}
	if (parts.length === 0) return 'Every 0 seconds';
	if (parts.length === 1 && parts[0][0] === 1) return `Every ${parts[0][1]}`;
	return `Every ${parts.map(([count, unit]) => `${number(count)} ${unit}${count === 1 ? '' : 's'}`).join(' ')}`;
}

const CDN = 'https://cdn.discordapp.com';

/** The guild's icon on Discord's CDN, or nothing when it has none. */
export function discordIcon(guild: Snowflake, hash: string | null, size = 64): string | null {
	if (!hash) return null;
	const ext = hash.startsWith('a_') ? 'gif' : 'png';
	return `${CDN}/icons/${guild}/${hash}.${ext}?size=${size}`;
}

/** The user's avatar on Discord's CDN, or the default avatar Discord gives them. */
export function discordAvatar(user: Snowflake, hash: string | null, size = 64): string {
	if (hash) {
		const ext = hash.startsWith('a_') ? 'gif' : 'png';
		return `${CDN}/avatars/${user}/${hash}.${ext}?size=${size}`;
	}
	let index: number;
	try {
		index = Number((BigInt(user) >> 22n) % 6n);
	} catch {
		index = 0;
	}
	return `${CDN}/embed/avatars/${index}.png`;
}

const MEDIA_LABELS: Record<MediaKind, string> = {
	video: 'Video',
	audio: 'Audio',
	image: 'Image',
	file: 'File'
};

/** The word for a media kind. */
export function mediaLabel(kind: MediaKind): string {
	return MEDIA_LABELS[kind];
}

const STAGE_LABELS: Record<Stage, string> = {
	resolve: 'Resolve',
	download: 'Download',
	transcode: 'Transcode',
	publish: 'Publish',
	archive: 'Archive'
};

/** The word for a pipeline stage, capitalised for labels and states. */
export function stageLabel(stage: Stage): string {
	return STAGE_LABELS[stage];
}

/** The limits in force as one line: each one set, or word that the engine's own apply. */
export function limitsText(limits: RequestLimits): string {
	const parts: string[] = [];
	if (limits.max_source_bytes !== null)
		parts.push(`Source up to ${bytes(limits.max_source_bytes)}`);
	if (limits.max_duration_secs !== null) {
		parts.push(
			limits.max_duration_secs === 0
				? 'No live streams'
				: `Length up to ${clock(limits.max_duration_secs)}`
		);
	}
	if (limits.max_height !== null) parts.push(`Height up to ${limits.max_height} px`);
	return parts.length > 0 ? parts.join(' · ') : 'The engine’s own';
}

/** The first block of a UUID, or the whole of any other identifier, for showing beside a copy button. */
export function shortId(id: string): string {
	const match = /^([0-9a-f]{8})-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.exec(id);
	return match ? match[1] : id;
}

/** The first letters of a name, for avatars without a picture. */
export function initials(name: string): string {
	const parts = name.trim().split(/\s+/).filter(Boolean);
	if (parts.length === 0) return '?';
	if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
	return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

/** The host of a URL, without `www.`. */
export function host(url: string | null | undefined): string {
	if (!url) return '';
	try {
		return new URL(url).host.replace(/^www\./, '');
	} catch {
		return url;
	}
}

/** Seconds from `90`, `1:30` or `1:02:03`. Anything else is `null`. */
export function parseClock(text: string): number | null {
	const trimmed = text.trim();
	if (!trimmed) return null;
	const parts = trimmed.split(':');
	if (parts.length > 3 || parts.some((p) => !/^\d+(\.\d+)?$/.test(p))) return null;
	let total = 0;
	for (const part of parts) total = total * 60 + Number(part);
	return Number.isFinite(total) ? total : null;
}
