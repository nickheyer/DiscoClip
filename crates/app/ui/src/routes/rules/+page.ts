import type { PageLoad } from './$types';
import { applications, rules } from '$lib/api';
import { guarded, optional, requirePermission } from '$lib/api/load';
import type { ApplicationView, BotGuild } from '$lib/api';

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
	return { rules: list, apps, guildsByApp };
};
