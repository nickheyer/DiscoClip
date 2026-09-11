// Turns API failures inside `load` functions into what SvelteKit renders.

import { error, isHttpError, isRedirect, redirect } from '@sveltejs/kit';
import { isApiError, messageOf } from './client';
import type { Permission } from './types';
import { session } from '$lib/state/session.svelte';
import { PERMISSION_LABELS } from '$lib/permissions';

/** Runs the call; a failure becomes the error page, and a lost session the login page. */
export async function guarded<T>(run: () => Promise<T>): Promise<T> {
	try {
		return await run();
	} catch (cause) {
		if (isRedirect(cause) || isHttpError(cause)) throw cause;
		if (isApiError(cause)) {
			if (cause.status === 401) redirect(307, '/login');
			error(cause.status === 0 ? 503 : cause.status, cause.message);
		}
		throw cause;
	}
}

/**
 * The same call, but an answer in `absent` becomes `null`: for what the account may not
 * be allowed to see next to what it may. Anything else fails the page like `guarded`.
 */
export async function optional<T>(
	run: () => Promise<T>,
	absent: number[] = [403, 404]
): Promise<T | null> {
	try {
		return await run();
	} catch (cause) {
		if (isRedirect(cause) || isHttpError(cause)) throw cause;
		if (isApiError(cause)) {
			if (cause.status === 401) redirect(307, '/login');
			if (absent.includes(cause.status)) return null;
			error(cause.status === 0 ? 503 : cause.status, cause.message);
		}
		throw cause;
	}
}

export type Settled<T> = { ok: true; value: T } | { ok: false; error: string };

/** The call's answer or its failure, for pages that show each part on its own. */
export async function settle<T>(run: () => Promise<T>): Promise<Settled<T>> {
	try {
		return { ok: true, value: await run() };
	} catch (cause) {
		if (isRedirect(cause) || isHttpError(cause)) throw cause;
		if (isApiError(cause) && cause.status === 401) redirect(307, '/login');
		return { ok: false, error: messageOf(cause) };
	}
}

type Parent = () => Promise<unknown>;

/**
 * Page loads run alongside the layout's, so a page waits for the layout, which is what
 * fetches the session, before asking who is logged in.
 */
export async function requireSession(parent: Parent): Promise<void> {
	await parent();
	if (!session.me) redirect(307, '/login');
}

export async function requirePermission(parent: Parent, permission: Permission): Promise<void> {
	await requireSession(parent);
	if (!session.can(permission)) {
		error(
			403,
			`The ${session.role} role does not allow ${PERMISSION_LABELS[permission].label.toLowerCase()}.`
		);
	}
}
