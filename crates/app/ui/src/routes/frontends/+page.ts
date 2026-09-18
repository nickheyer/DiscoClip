import type { PageLoad } from './$types';
import { frontends, profiles, providers } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:frontends');
	await requirePermission(parent, 'manage_settings');
	const [list, profileList, providerList] = await Promise.all([
		guarded(frontends.list),
		guarded(profiles.list),
		guarded(providers.list)
	]);
	return { frontends: list, profiles: profileList, providers: providerList };
};
