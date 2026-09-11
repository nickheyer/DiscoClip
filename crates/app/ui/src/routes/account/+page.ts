import type { PageLoad } from './$types';
import { auth, providers, tokens } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:account');
	await requireSession(parent);
	const [sessions, apiTokens, identities, offered] = await guarded(() =>
		Promise.all([auth.sessions(), tokens.list(), providers.identities(), providers.list()])
	);
	return { sessions, tokens: apiTokens, identities, providers: offered };
};
