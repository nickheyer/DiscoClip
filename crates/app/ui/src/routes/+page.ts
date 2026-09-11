import type { PageLoad } from './$types';
import { applications, audit, guilds, rules, users } from '$lib/api';
import { requireSession, settle } from '$lib/api/load';
import { session } from '$lib/state/session.svelte';

export const load: PageLoad = async ({ depends, parent }) => {
	depends('app:overview');
	await requireSession(parent);
	const [apps, allRules, accounts, myGuilds, recent] = await Promise.all([
		session.can('manage_applications') ? settle(applications.list) : null,
		session.can('manage_watch_rules') ? settle(rules.listAll) : null,
		session.can('manage_users') ? settle(users.list) : null,
		settle(guilds.list),
		session.can('view_audit_log') ? settle(() => audit.list({ limit: 8 })) : null
	]);
	return { apps, rules: allRules, accounts, guilds: myGuilds, recent };
};
