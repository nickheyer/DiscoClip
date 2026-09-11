import type { PageLoad } from './$types';
import { ACTIONS, TARGET_KINDS, audit, users } from '$lib/api';
import type { Action, AuditQuery, TargetKind } from '$lib/api';
import { guarded, optional, requirePermission } from '$lib/api/load';

export const LIMITS = [25, 50, 100, 250, 500];

/** The filters as the address names them, dropping anything the API would not accept. */
export function queryFrom(params: URLSearchParams): AuditQuery {
	const action = params.get('action');
	const kind = params.get('target_kind');
	const limit = Number(params.get('limit'));
	const query: AuditQuery = {
		actor: params.get('actor') || undefined,
		action: action && ACTIONS.includes(action as Action) ? (action as Action) : undefined,
		target_kind: kind && TARGET_KINDS.includes(kind as TargetKind) ? (kind as TargetKind) : undefined,
		target_id: params.get('target_id') || undefined,
		since: params.get('since') || undefined,
		until: params.get('until') || undefined,
		limit: LIMITS.includes(limit) ? limit : 50
	};
	if (!query.target_kind) query.target_id = undefined;
	return query;
}

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
