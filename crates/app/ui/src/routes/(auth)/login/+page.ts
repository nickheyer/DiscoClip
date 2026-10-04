// A logged-in account has no use for the login page.

import { redirect } from '@sveltejs/kit';
import { resolve } from '$app/paths';
import { auth, providers } from '$lib/api/endpoints';
import { session } from '$lib/session.svelte';
import type { PageLoad } from './$types';

export const load: PageLoad = async () => {
	const status = await auth.setupStatus();
	if (status.needed) redirect(307, resolve('/setup'));
	const who = await session.refresh();
	if (who) redirect(307, resolve('/'));
	return { providers: await providers.list() };
};
