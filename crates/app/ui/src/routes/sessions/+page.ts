import type { PageLoad } from './$types';
import { auth } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:sessions');
	await requirePermission(parent, 'manage_users');
	return { sessions: await guarded(auth.allSessions) };
};
