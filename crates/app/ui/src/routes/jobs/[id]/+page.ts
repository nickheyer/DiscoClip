import type { PageLoad } from './$types';
import { jobs } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:job:${params.id}`);
	await requireSession(parent);
	const job = await guarded(() => jobs.get(params.id));
	const children = job.artifacts.children.length ? await guarded(() => jobs.children(params.id)) : [];
	return { job, children };
};
