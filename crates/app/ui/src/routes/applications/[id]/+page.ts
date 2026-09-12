import type { PageLoad } from './$types';
import { applications, rules } from '$lib/api';
import { guarded, optional, requirePermission } from '$lib/api/load';

export const load: PageLoad = async ({ params, depends, parent }) => {
	depends(`app:application:${params.id}`);
	await requirePermission(parent, 'manage_applications');
	const [[app, guilds, commands, install], allRules] = await Promise.all([
		guarded(() =>
			Promise.all([
				applications.get(params.id),
				applications.guilds(params.id),
				applications.commands(params.id),
				applications.install(params.id)
			])
		),
		optional(rules.listAll)
	]);
	// How many rules each guild has, for the guild list; `null` when rules may not be read.
	const ruleCounts = allRules
		? new Map<string, number>(
				guilds.map((guild) => [
					guild.guild_id,
					allRules.filter((rule) => rule.application_id === params.id && rule.guild_id === guild.guild_id).length
				])
			)
		: null;
	return { app, guilds, commands, install, ruleCounts };
};
