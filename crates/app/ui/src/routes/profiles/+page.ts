import type { PageLoad } from './$types';
import { platforms, profiles } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:profiles');
	await requireSession(parent);
	const [list, coverage, presets] = await Promise.all([
		guarded(profiles.list),
		guarded(platforms.list),
		guarded(profiles.presets)
	]);
	// The whole server's assignment is readable by anyone who can see profiles; it is what
	// the effective profile with no guild is made of.
	const effective = await guarded(() => profiles.effective());
	const global = effective.applied.find((a) => a.scope.kind === 'global') ?? null;
	return { profiles: list, platforms: coverage, presets, global };
};
