import type { PageLoad } from './$types';
import { applications, guilds, rules } from '$lib/api';
import { guarded, optional, requireSession } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:guild:${params.id}:${params.guild}`);
	await requireSession(parent);
	const [list, app, botGuilds, myGuilds, install] = await Promise.all([
		guarded(() => rules.listForGuild(params.id, params.guild)),
		optional(() => applications.get(params.id)),
		optional(() => applications.guilds(params.id)),
		guarded(guilds.list),
		optional(() => applications.install(params.id, params.guild))
	]);
	return {
		applicationId: params.id,
		guildId: params.guild,
		rules: list,
		app,
		botGuild: botGuilds?.find((g) => g.guild_id === params.guild) ?? null,
		myGuild: myGuilds.find((g) => g.id === params.guild) ?? null,
		install
	};
};
