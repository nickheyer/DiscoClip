// Setup happens once. Afterwards the page sends the browser to log in.

import { redirect } from '@sveltejs/kit';
import { resolve } from '$app/paths';
import { auth } from '$lib/api/endpoints';
import type { PageLoad } from './$types';

export const load: PageLoad = async () => {
	const status = await auth.setupStatus();
	if (!status.needed) redirect(307, resolve('/login'));
	return {};
};
