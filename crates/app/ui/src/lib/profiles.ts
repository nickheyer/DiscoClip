// Profile labels and previews.

import type {
	Assignment,
	PlatformCoverage,
	PlatformDefault,
	PlatformToggles,
	Preset,
	Profile,
	ProfileInput,
	ProfileLimits,
	RequestLimits,
	Scope
} from './api/types';
import type { GuildChannel } from './api/types';
import { channelName } from './discord';
import { formatBytes, formatDuration } from './format';

export const DEFAULT_LABELS: Record<PlatformDefault, { label: string; hint: string }> = {
	inherit: {
		label: 'Inherit settings',
		hint: 'Use the inherited setting unless overridden below.'
	},
	enabled: {
		label: 'Turn every other platform on',
		hint: 'Enable all platforms unless disabled below.'
	},
	disabled: {
		label: 'Turn every other platform off',
		hint: 'Disable all platforms unless enabled below.'
	}
};

/** A platform's setting in a profile: following the default, on, or off. */
export type Toggle = 'inherit' | 'on' | 'off';

export function toggleOf(toggles: PlatformToggles, platform: string): Toggle {
	const value = toggles.overrides[platform];
	if (value === undefined) return 'inherit';
	return value ? 'on' : 'off';
}

/** The toggles with `platform` set to `toggle`, leaving the rest. */
export function withToggle(
	toggles: PlatformToggles,
	platform: string,
	toggle: Toggle
): PlatformToggles {
	const overrides = { ...toggles.overrides };
	if (toggle === 'inherit') delete overrides[platform];
	else overrides[platform] = toggle === 'on';
	return { ...toggles, overrides };
}

/** Whether the presets, not the default, decide the platforms the profile does not name. */
export function usesPresets(toggles: PlatformToggles): boolean {
	return toggles.presets.length > 0;
}

/** The platforms in any of the chosen presets: the whitelist, when presets are chosen. */
export function presetPlatforms(toggles: PlatformToggles, presets: Preset[]): Set<string> {
	const allowed = new Set<string>();
	for (const id of toggles.presets) {
		for (const platform of presets.find((p) => p.id === id)?.platforms ?? []) allowed.add(platform);
	}
	return allowed;
}

/** The toggles with `preset` chosen or not, leaving the rest. */
export function withPreset(toggles: PlatformToggles, preset: string, chosen: boolean): PlatformToggles {
	const presets = toggles.presets.filter((p) => p !== preset);
	if (chosen) presets.push(preset);
	return { ...toggles, presets };
}

/** Compute platform access: true enables, false disables, null inherits. Explicit overrides take priority over presets. */
export function preview(
	toggles: PlatformToggles,
	platforms: string[],
	presets: Preset[]
): Record<string, boolean | null> {
	const out: Record<string, boolean | null> = {};
	const whitelist = usesPresets(toggles) ? presetPlatforms(toggles, presets) : null;
	for (const platform of platforms) {
		const value = toggles.overrides[platform];
		if (value !== undefined) out[platform] = value;
		else if (whitelist) out[platform] = whitelist.has(platform);
		else out[platform] = toggles.default === 'inherit' ? null : toggles.default === 'enabled';
	}
	return out;
}

/** How many platforms a profile turns on, off, and leaves alone, across `platforms`. */
export function tally(
	toggles: PlatformToggles,
	platforms: string[],
	presets: Preset[]
): { on: number; off: number; inherit: number } {
	const seen = preview(toggles, platforms, presets);
	let on = 0;
	let off = 0;
	let inherit = 0;
	for (const value of Object.values(seen)) {
		if (value === true) on += 1;
		else if (value === false) off += 1;
		else inherit += 1;
	}
	return { on, off, inherit };
}

export const SCOPE_KIND_LABELS: Record<Scope['kind'], string> = {
	global: 'Global default',
	guild: 'Guild',
	channel: 'Channel',
	user: 'User'
};

/** Format an assignment scope. */
export function describeScope(scope: Scope, channels?: GuildChannel[] | null): string {
	switch (scope.kind) {
		case 'global':
			return 'the whole server';
		case 'guild':
			return `server ${scope.guild_id}`;
		case 'channel':
			return channels?.some((c) => c.id === scope.channel_id)
				? `#${channelName(channels, scope.channel_id)}`
				: `channel ${scope.channel_id}`;
		case 'user':
			return `user ${scope.user_id}`;
	}
}

export function profileName(profiles: Profile[], id: string): string {
	return profiles.find((p) => p.id === id)?.name ?? `Profile ${id.slice(0, 8)}`;
}

export function emptyLimits(): ProfileLimits {
	return { max_source_bytes: null, max_duration_secs: null, max_height: null };
}

export function emptyProfile(): ProfileInput {
	return {
		name: '',
		description: '',
		platforms: { default: 'inherit', presets: [], overrides: {} },
		limits: emptyLimits()
	};
}

/** A stored profile as an input again, copied so edits do not touch the original. */
export function toInput(profile: Profile): ProfileInput {
	return {
		name: profile.name,
		description: profile.description,
		platforms: {
			default: profile.platforms.default,
			presets: [...profile.platforms.presets],
			overrides: { ...profile.platforms.overrides }
		},
		limits: { ...profile.limits }
	};
}

export function cleanProfile(input: ProfileInput): ProfileInput {
	return {
		name: input.name.trim(),
		description: input.description.trim(),
		platforms: {
			default: input.platforms.default,
			presets: [...input.platforms.presets].sort(),
			overrides: Object.fromEntries(
				Object.entries(input.platforms.overrides).sort(([a], [b]) => a.localeCompare(b))
			)
		},
		limits: {
			max_source_bytes: input.limits.max_source_bytes ?? null,
			max_duration_secs: input.limits.max_duration_secs ?? null,
			max_height: input.limits.max_height ?? null
		}
	};
}

export function sameProfile(a: ProfileInput, b: ProfileInput): boolean {
	return JSON.stringify(cleanProfile(a)) === JSON.stringify(cleanProfile(b));
}

/** Format effective media limits. */
export function describeLimits(limits: ProfileLimits | RequestLimits, none = 'Inherited'): string {
	const parts: string[] = [];
	if (limits.max_source_bytes != null) parts.push(`≤ ${formatBytes(limits.max_source_bytes)}`);
	if (limits.max_duration_secs != null) {
		parts.push(limits.max_duration_secs === 0 ? 'no live streams' : `≤ ${formatDuration(limits.max_duration_secs)}`);
	}
	if (limits.max_height != null) parts.push(`≤ ${limits.max_height}p`);
	return parts.length ? parts.join(' · ') : none;
}

/** Summarize platform access and exceptions. */
export function describePlatforms(
	toggles: PlatformToggles,
	platforms: PlatformCoverage[],
	presets: Preset[]
): string {
	const named = Object.keys(toggles.overrides).length;
	const exceptions = named ? `, ${named} named` : '';
	if (usesPresets(toggles)) {
		const labels = toggles.presets.map((id) => presets.find((p) => p.id === id)?.label ?? id);
		return `Only ${labels.join(', ')}${exceptions}`;
	}
	const t = tally(toggles, platforms.map((p) => p.id), presets);
	switch (toggles.default) {
		case 'enabled':
			return named ? `All on but ${t.off} off` : 'Every platform on';
		case 'disabled':
			return named ? `All off but ${t.on} on` : 'Every platform off';
		default:
			return named ? `Inherited, ${t.on} on and ${t.off} off` : 'Inherited';
	}
}

/** Find the channel assignment, then the server assignment, then the global default. */
export function inForceFor(assignments: Assignment[] | null, channel?: string): Assignment | null {
	if (!assignments) return null;
	if (channel) {
		const own = assignments.find((a) => a.scope.kind === 'channel' && a.scope.channel_id === channel);
		if (own) return own;
	}
	return (
		assignments.find((a) => a.scope.kind === 'guild') ??
		assignments.find((a) => a.scope.kind === 'global') ??
		null
	);
}

/** How many scopes a profile is assigned at, by kind. */
export function inForceCount(assignments: Assignment[] | null, profile: string): { total: number; text: string } {
	if (!assignments) return { total: 0, text: '' };
	const mine = assignments.filter((a) => a.profile_id === profile);
	const kinds: Record<string, number> = {};
	for (const a of mine) kinds[a.scope.kind] = (kinds[a.scope.kind] ?? 0) + 1;
	const parts: string[] = [];
	if (kinds.global) parts.push('the whole server');
	if (kinds.guild) parts.push(`${kinds.guild} guild${kinds.guild === 1 ? '' : 's'}`);
	if (kinds.channel) parts.push(`${kinds.channel} channel${kinds.channel === 1 ? '' : 's'}`);
	if (kinds.user) parts.push(`${kinds.user} member${kinds.user === 1 ? '' : 's'}`);
	return { total: mine.length, text: parts.join(', ') };
}
