import type { PageLoad } from './$types';
import { jobs } from '$lib/api';
import { guarded, requireSession, settle } from '$lib/api/load';
import { queryFrom } from './query';

export const load: PageLoad = async ({ url, depends, parent }) => {
	depends('app:jobs');
	await requireSession(parent);
	const query = queryFrom(url.searchParams);
	const [page, stats] = await Promise.all([guarded(() => jobs.list(query)), settle(jobs.stats)]);
	return { page, query, stats };
};
