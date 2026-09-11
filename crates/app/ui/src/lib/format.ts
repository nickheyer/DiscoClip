// Formatting shared by every page: times, sizes, ids and Discord bits.

const dateTime = new Intl.DateTimeFormat(undefined, {
	dateStyle: 'medium',
	timeStyle: 'short'
});

const dateTimeSeconds = new Intl.DateTimeFormat(undefined, {
	dateStyle: 'medium',
	timeStyle: 'medium'
});

const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

export function formatDateTime(iso: string, seconds = false): string {
	const date = new Date(iso);
	if (Number.isNaN(date.getTime())) return iso;
	return (seconds ? dateTimeSeconds : dateTime).format(date);
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
	['year', 365 * 24 * 3600],
	['month', 30 * 24 * 3600],
	['week', 7 * 24 * 3600],
	['day', 24 * 3600],
	['hour', 3600],
	['minute', 60]
];

/** "3 minutes ago", "in 2 hours", "now". */
export function formatRelative(iso: string, now: number = Date.now()): string {
	const date = new Date(iso);
	if (Number.isNaN(date.getTime())) return iso;
	const delta = (date.getTime() - now) / 1000;
	const magnitude = Math.abs(delta);
	// Within clock skew of now, either way, is now.
	if (magnitude < 45) return 'just now';
	for (const [unit, size] of UNITS) {
		if (magnitude >= size) return relativeFormat.format(Math.round(delta / size), unit);
	}
	return relativeFormat.format(Math.round(delta), 'second');
}

/** "1m 05s", "2h 10m", "45s". */
export function formatDuration(totalSeconds: number): string {
	const seconds = Math.max(0, Math.round(totalSeconds));
	const h = Math.floor(seconds / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	const s = seconds % 60;
	if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m`;
	if (m > 0) return `${m}m ${String(s).padStart(2, '0')}s`;
	return `${s}s`;
}

/** Seconds until `iso`, never negative. */
export function secondsUntil(iso: string, now: number = Date.now()): number {
	const date = new Date(iso).getTime();
	if (Number.isNaN(date)) return 0;
	return Math.max(0, Math.ceil((date - now) / 1000));
}

export function formatBytes(bytes: number): string {
	if (!Number.isFinite(bytes)) return String(bytes);
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	let value = bytes;
	let index = 0;
	while (value >= 1024 && index < units.length - 1) {
		value /= 1024;
		index += 1;
	}
	const digits = index === 0 ? 0 : value >= 100 ? 0 : value >= 10 ? 1 : 2;
	return `${value.toFixed(digits)} ${units[index]}`;
}

export const numberFormat = new Intl.NumberFormat();

export function formatNumber(value: number): string {
	return numberFormat.format(value);
}

/** The first group of a UUID, enough to tell ids apart at a glance. */
export function shortId(id: string): string {
	return id.split('-')[0] ?? id;
}

export const SNOWFLAKE = /^[1-9][0-9]{6,21}$/;

export function isSnowflake(value: string): boolean {
	return SNOWFLAKE.test(value.trim());
}

/** A guild's icon on Discord's CDN, animated ones as GIFs. */
export function guildIconUrl(guildId: string, icon: string | null, size = 64): string | null {
	if (!icon) return null;
	const ext = icon.startsWith('a_') ? 'gif' : 'png';
	return `https://cdn.discordapp.com/icons/${guildId}/${icon}.${ext}?size=${size}`;
}

/** Up to two letters for an avatar without an image. */
export function initials(name: string): string {
	const words = name
		.split(/\s+/)
		.map((w) => w.trim())
		.filter(Boolean);
	if (words.length === 0) return '?';
	if (words.length === 1) return words[0]!.slice(0, 2).toUpperCase();
	return (words[0]![0]! + words[words.length - 1]![0]!).toUpperCase();
}

/** Browser and platform words out of a user agent string. */
export function describeUserAgent(agent: string | null): string {
	if (!agent) return 'Unknown client';
	const ua = agent;
	let browser = 'Browser';
	if (/Edg\//.test(ua)) browser = 'Edge';
	else if (/OPR\//.test(ua)) browser = 'Opera';
	else if (/Firefox\//.test(ua)) browser = 'Firefox';
	else if (/Chrome\//.test(ua)) browser = 'Chrome';
	else if (/Safari\//.test(ua) && /Version\//.test(ua)) browser = 'Safari';
	else if (/curl\//.test(ua)) browser = 'curl';
	else browser = ua.split(' ')[0]?.split('/')[0] ?? 'Client';
	let platform = '';
	if (/Windows/.test(ua)) platform = 'Windows';
	else if (/Android/.test(ua)) platform = 'Android';
	else if (/iPhone|iPad/.test(ua)) platform = 'iOS';
	else if (/Mac OS X/.test(ua)) platform = 'macOS';
	else if (/CrOS/.test(ua)) platform = 'ChromeOS';
	else if (/Linux/.test(ua)) platform = 'Linux';
	return platform ? `${browser} on ${platform}` : browser;
}

export function pluralize(count: number, singular: string, plural = `${singular}s`): string {
	return `${formatNumber(count)} ${count === 1 ? singular : plural}`;
}

/** A safe in-app path to return to after logging in. */
export function safeNext(value: string | null): string {
	if (!value || !value.startsWith('/') || value.startsWith('//')) return '/';
	if (value === '/login' || value === '/setup') return '/';
	return value;
}

export function prettyJson(value: unknown): string {
	return JSON.stringify(value, null, 2) ?? 'null';
}

/** Seconds as `1:02:03` or `4:05`, the way players show a duration. */
export function formatClock(totalSeconds: number): string {
	const seconds = Math.max(0, Math.round(totalSeconds));
	const h = Math.floor(seconds / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	const s = seconds % 60;
	const mm = h > 0 ? String(m).padStart(2, '0') : String(m);
	return `${h > 0 ? `${h}:` : ''}${mm}:${String(s).padStart(2, '0')}`;
}

/** `1.5`, `90`, `1:30`, `1m30s` or `1h2m3s` as seconds; null when it is none of those. */
export function parseTimeStamp(text: string): number | null {
	const value = text.trim();
	if (!value) return null;
	if (value.includes(':')) {
		let total = 0;
		for (const part of value.split(':')) {
			const n = Number(part);
			if (!Number.isFinite(n) || n < 0) return null;
			total = total * 60 + n;
		}
		return total;
	}
	if (/^\d+(\.\d+)?$/.test(value)) return Number(value);
	const match = /^(?:(\d+(?:\.\d+)?)h)?(?:(\d+(?:\.\d+)?)m)?(?:(\d+(?:\.\d+)?)s)?$/.exec(value);
	if (!match || match[0] === '') return null;
	return Number(match[1] ?? 0) * 3600 + Number(match[2] ?? 0) * 60 + Number(match[3] ?? 0);
}
