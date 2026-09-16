// The audit page's filters, as the address names them.

import { ACTIONS, TARGET_KINDS } from '$lib/api';
import type { Action, AuditQuery, TargetKind } from '$lib/api';

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
