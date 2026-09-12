import type { PageLoad } from './$types';
import { applications, channels, rules } from '$lib/api';
import { guarded, optional, requirePermission, settle } from '$lib/api/load';
import type { ApplicationView, BotGuild, GuildChannel } from '$lib/api';
import { groupKey } from '$lib/discord';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:rules');
	await requirePermission(parent, 'manage_watch_rules');
	const [list, apps] = await Promise.all([guarded(rules.listAll), optional(applications.list)]);
	const guildsByApp = new Map<string, BotGuild[]>();
	if (apps) {
		const lists = await Promise.all(
			apps.map((app: ApplicationView) => optional(() => applications.guilds(app.id)))
		);
		apps.forEach((app: ApplicationView, i: number) => guildsByApp.set(app.id, lists[i] ?? []));
	}
	// Each guild's channels, so rules are shown by name; a guild the bot cannot list keeps
	// its error and its rules are shown by id.
	const pairs = new Map<string, { applicationId: string; guildId: string }>();
	for (const rule of list) {
		pairs.set(groupKey(rule.application_id, rule.guild_id), {
			applicationId: rule.application_id,
			guildId: rule.guild_id
		});
	}
	const channelsByGuild = new Map<string, GuildChannel[] | null>();
	const channelErrors = new Map<string, string>();
	const listed = await Promise.all(
		[...pairs.values()].map((pair) => settle(() => channels.list(pair.applicationId, pair.guildId)))
	);
	[...pairs.keys()].forEach((key, i) => {
		const outcome = listed[i]!;
		channelsByGuild.set(key, outcome.ok ? outcome.value : null);
		if (!outcome.ok) channelErrors.set(key, outcome.error);
	});
	return { rules: list, apps, guildsByApp, channelsByGuild, channelErrors };
};
