import type { PageLoad } from './$types';
import { STATUS_KINDS, jobs } from '$lib/api';
import type { JobOrder, JobQuery, StatusKind } from '$lib/api';
import { guarded, requireSession, settle } from '$lib/api/load';

export const LIMITS = [25, 50, 100, 250];

/** The filters as the address names them, dropping anything the API would not accept. */
export function queryFrom(params: URLSearchParams): JobQuery {
	const status = params.get('status');
	const order = params.get('order');
	const limit = Number(params.get('limit'));
	const offset = Number(params.get('offset'));
	return {
		q: params.get('q') || undefined,
		status: status && STATUS_KINDS.includes(status as StatusKind) ? (status as StatusKind) : undefined,
		source: params.get('source') || undefined,
		resolver: params.get('resolver') || undefined,
		parent: params.get('parent') || undefined,
		top_level: params.get('top_level') === 'true' ? true : undefined,
		order: order === 'oldest' ? ('oldest' as JobOrder) : undefined,
		limit: LIMITS.includes(limit) ? limit : 50,
		offset: Number.isInteger(offset) && offset > 0 ? offset : undefined
	};
}

export const load: PageLoad = async ({ url, depends, parent }) => {
	depends('app:jobs');
	await requireSession(parent);
	const query = queryFrom(url.searchParams);
	const [page, stats] = await Promise.all([guarded(() => jobs.list(query)), settle(jobs.stats)]);
	return { page, query, stats };
};
