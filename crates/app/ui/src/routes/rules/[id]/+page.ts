import type { PageLoad } from './$types';
import { applications, rules } from '$lib/api';
import { guarded, optional, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:rule:${params.id}`);
	await requireSession(parent);
	const rule = await guarded(() => rules.get(params.id));
	const [app, botGuilds] = await Promise.all([
		optional(() => applications.get(rule.application_id)),
		optional(() => applications.guilds(rule.application_id))
	]);
	return {
		rule,
		app,
		botGuild: botGuilds?.find((g) => g.guild_id === rule.guild_id) ?? null
	};
};
