import type { PageLoad } from './$types';
import { health } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:health');
	await requireSession(parent);
	return { health: await guarded(health.get) };
};
