// Every operator page needs an account. Without one the browser goes to setup or login.

import { redirect } from '@sveltejs/kit';
import { resolve } from '$app/paths';
import { auth, providers } from '$lib/api/endpoints';
import { session } from '$lib/session.svelte';
import type { LayoutLoad } from './$types';

export const load: LayoutLoad = async () => {
	const status = await auth.setupStatus();
	if (status.needed) redirect(307, resolve('/setup'));
	const who = await session.refresh();
	if (!who) {
		const next = location.pathname + location.search;
		if (next === '/') redirect(307, resolve('/login'));
		// The query rides on the resolved path.
		redirect(307, `${resolve('/login')}?next=${encodeURIComponent(next)}`);
	}
	const identities = await providers.identities();
	return { who, hasDiscord: identities.some((identity) => identity.provider === 'discord') };
};
