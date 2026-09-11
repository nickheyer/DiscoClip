import { redirect } from '@sveltejs/kit';
import type { LayoutLoad } from './$types';
import { guarded } from '$lib/api/load';
import { safeNext } from '$lib/format';
import { session } from '$lib/state/session.svelte';
import { setupNeeded } from '$lib/state/setup';

// The app is a single page served by the DiscoClip binary; the API is the only server side.
export const ssr = false;
export const prerender = false;

export const load: LayoutLoad = async ({ url }) => {
	const path = url.pathname;
	const needed = await guarded(setupNeeded);
	if (needed) {
		if (path !== '/setup') redirect(307, '/setup');
		return { me: null };
	}
	if (path === '/setup') redirect(307, session.me ? '/' : '/login');
	if (!session.loaded) await guarded(() => session.load());
	const me = session.me;
	if (!me && path !== '/login') {
		const next = `${path}${url.search}`;
		redirect(307, next === '/' ? '/login' : `/login?next=${encodeURIComponent(next)}`);
	}
	if (me && path === '/login' && !session.ending) {
		redirect(307, safeNext(url.searchParams.get('next')));
	}
	return { me };
};
