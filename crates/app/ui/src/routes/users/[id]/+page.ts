import type { PageLoad } from './$types';
import { users } from '$lib/api';
import { guarded, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:user:${params.id}`);
	await requirePermission(parent, 'manage_users');
	const [user, sessions, tokens] = await guarded(() =>
		Promise.all([users.get(params.id), users.sessions(params.id), users.tokens(params.id)])
	);
	return { user, sessions, tokens };
};
