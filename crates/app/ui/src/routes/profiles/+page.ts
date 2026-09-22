import type { PageLoad } from './$types';
import { platforms, profiles } from '$lib/api';
import { guarded, optional, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:profiles');
	await requireSession(parent);
	const [list, coverage, presets, assignments] = await Promise.all([
		guarded(profiles.list),
		guarded(platforms.list),
		guarded(profiles.presets),
		// Where each profile is assigned, for accounts allowed to see every server's.
		optional(() => profiles.assignments(), [403])
	]);
	// The effective global profile is readable without assignment permissions.
	const effective = await guarded(() => profiles.effective());
	const global = effective.applied.find((a) => a.scope.kind === 'global') ?? null;
	return { profiles: list, platforms: coverage, presets, assignments, global };
};
