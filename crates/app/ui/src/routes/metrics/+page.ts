import type { PageLoad } from './$types';
import { metrics } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:metrics');
	await requireSession(parent);
	return { metrics: await guarded(metrics.get) };
};
