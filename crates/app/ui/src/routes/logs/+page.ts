import type { PageLoad } from './$types';
import { logs } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ parent }) => {
	await requirePermission(parent, 'view_logs');
	return { first: await guarded(() => logs.list({ limit: 200 })) };
};
