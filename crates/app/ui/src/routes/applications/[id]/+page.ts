import type { PageLoad } from './$types';
import { applications } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:application:${params.id}`);
	await requirePermission(parent, 'manage_applications');
	const [app, guilds, commands, install] = await guarded(() =>
		Promise.all([
			applications.get(params.id),
			applications.guilds(params.id),
			applications.commands(params.id),
			applications.install(params.id)
		])
	);
	return { app, guilds, commands, install };
};
