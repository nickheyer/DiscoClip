// What the profiles pages say about profiles, scopes and what they amount to.

import type { PlatformDefault, PlatformToggles, Preset, Profile, Scope } from './api/types';
import type { GuildChannel } from './api/types';
import { channelName } from './discord';

export const DEFAULT_LABELS: Record<PlatformDefault, { label: string; hint: string }> = {
	inherit: {
		label: 'Leave as the wider scope has them',
		hint: 'Platforms this profile does not name stay as the server, guild or channel it sits under allows them.'
	},
	enabled: {
		label: 'Turn every other platform on',
		hint: 'Platforms this profile does not name are on, whatever the wider scope says.'
	},
	disabled: {
		label: 'Turn every other platform off',
		hint: 'Platforms this profile does not name are off; only the ones set to on here are taken.'
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

/**
 * What a profile amounts to on its own, per platform: `true` on, `false` off, `null` left
 * to the wider scope. With presets chosen, a platform is on when in any of them and off
 * otherwise, as the server applies them; overrides win either way.
 */
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
	global: 'Whole server',
	guild: 'Guild',
	channel: 'Channel',
	user: 'User'
};

/** A scope in words, naming channels when the guild's channels are known. */
export function describeScope(scope: Scope, channels?: GuildChannel[] | null): string {
	switch (scope.kind) {
		case 'global':
			return 'the whole server';
		case 'guild':
			return `guild ${scope.guild_id}`;
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

export function emptyProfile(): { name: string; description: string; platforms: PlatformToggles } {
	return { name: '', description: '', platforms: { default: 'inherit', presets: [], overrides: {} } };
}
