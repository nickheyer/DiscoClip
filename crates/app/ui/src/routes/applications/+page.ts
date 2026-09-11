import type { PageLoad } from './$types';
import { applications } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:applications');
	await requirePermission(parent, 'manage_applications');
	return { apps: await guarded(applications.list) };
};
