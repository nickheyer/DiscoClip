import type { PageLoad } from './$types';
import { applications, audit, guilds, jobs, rules, users } from '$lib/api';
import { requireSession, settle } from '$lib/api/load';
import { session } from '$lib/state/session.svelte';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:overview');
	await requireSession(parent);
	const [stats, recent, apps, allRules, accounts, myGuilds, activity] = await Promise.all([
		settle(jobs.stats),
		settle(() => jobs.list({ limit: 12 })),
		session.can('manage_applications') ? settle(applications.list) : null,
		session.can('manage_watch_rules') ? settle(rules.listAll) : null,
		session.can('manage_users') ? settle(users.list) : null,
		settle(guilds.list),
		session.can('view_audit_log') ? settle(() => audit.list({ limit: 8 })) : null
	]);
	return { stats, recent, apps, rules: allRules, accounts, guilds: myGuilds, activity };
};
