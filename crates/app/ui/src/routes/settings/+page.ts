import type { PageLoad } from './$types';
import { settings } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ url, depends, parent }) => {
	depends('app:settings');
	await requirePermission(parent, 'manage_settings');
	return { view: await guarded(settings.get), key: url.searchParams.get('key') };
};
