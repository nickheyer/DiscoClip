import type { PageLoad } from './$types';
import { users } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:users');
	await requirePermission(parent, 'manage_users');
	return { users: await guarded(users.list) };
};
