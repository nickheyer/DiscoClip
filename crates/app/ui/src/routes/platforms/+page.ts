import type { PageLoad } from './$types';
import { platforms } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:platforms');
	await requireSession(parent);
	return { platforms: await guarded(platforms.list) };
};
