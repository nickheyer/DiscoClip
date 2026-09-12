import type { PageLoad } from './$types';
import { tokens } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:tokens');
	await requirePermission(parent, 'manage_users');
	return { tokens: await guarded(tokens.all) };
};
