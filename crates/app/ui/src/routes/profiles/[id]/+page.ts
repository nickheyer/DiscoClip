import type { PageLoad } from './$types';
import { guilds, platforms, profiles } from '$lib/api';
import { guarded, optional, requireSession } from '$lib/api/load';

/** The editor for one profile, or for a new one at `/profiles/new`. */
export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:profile:${params.id}`);
	await requireSession(parent);
	const isNew = params.id === 'new';
	const [profile, coverage, presets, assignments, myGuilds, effective] = await Promise.all([
		isNew ? Promise.resolve(null) : guarded(() => profiles.get(params.id)),
		guarded(platforms.list),
		guarded(profiles.presets),
		// Where the profile is assigned, for accounts allowed to see every server's.
		optional(() => profiles.assignments(), [403]),
		guarded(guilds.list),
		guarded(() => profiles.effective())
	]);
	return {
		id: params.id,
		profile,
		platforms: coverage,
		presets,
		assignments,
		guilds: myGuilds,
		global: effective.applied.find((a) => a.scope.kind === 'global') ?? null
	};
};
