import type { PageLoad } from './$types';
import { guilds, providers } from '$lib/api';
import { guarded, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:guilds');
	await requireSession(parent);
	const [list, identities, offered] = await guarded(() =>
		Promise.all([guilds.list(), providers.identities(), providers.list()])
	);
	return {
		guilds: list,
		discordLinked: identities.some((i) => i.provider === 'discord'),
		discordOffered: offered.some((p) => p.id === 'discord')
	};
};
