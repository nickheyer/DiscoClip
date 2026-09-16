import type { PageLoad } from './$types';
import { audit, users } from '$lib/api';
import { guarded, optional, requirePermission } from '$lib/api/load';
import { queryFrom } from './query';

export const load: PageLoad = async ({ url, depends, parent }) => {
	depends('app:audit');
	await requirePermission(parent, 'view_audit_log');
	const query = queryFrom(url.searchParams);
	const [page, accounts] = await Promise.all([
		guarded(() => audit.list(query)),
		optional(users.list)
	]);
	return { page, query, accounts: accounts ?? [] };
};
