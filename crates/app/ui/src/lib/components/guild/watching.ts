// What the watch switches do: a rule per channel switched on, a rule without a channel for
// a server watched whole, and under that, a channel rule switched off to leave a channel out.

import { rules as rulesApi } from '$lib/api/endpoints';
import type { GuildChannel, Rule, RuleInput, Snowflake, Uuid } from '$lib/api/types';
import { number } from '$lib/format';
import { channelLabel } from './channels';

/** Whether a rule says more than which channel it watches. */
export function hasOptions(rule: Rule): boolean {
	return (
		(rule.post_to !== null && rule.post_to !== rule.channel_id) ||
		rule.allow_users.length > 0 ||
		rule.allow_roles.length > 0
	);
}

/** A rule's options in a few words: where the media goes, whose links count. */
export function ruleSummary(rule: Rule, channels: GuildChannel[]): string[] {
	const parts: string[] = [];
	if (rule.post_to !== null && rule.post_to !== rule.channel_id)
		parts.push(
			`to ${channelLabel(
				channels.find((c) => c.id === rule.post_to),
				rule.post_to
			)}`
		);
	if (rule.allow_users.length > 0)
		parts.push(
			`${number(rule.allow_users.length)} ${rule.allow_users.length === 1 ? 'member' : 'members'}`
		);
	if (rule.allow_roles.length > 0)
		parts.push(
			`${number(rule.allow_roles.length)} ${rule.allow_roles.length === 1 ? 'role' : 'roles'}`
		);
	return parts;
}

/** The rule as it would be sent back, to change one field of it. */
export function inputOf(rule: Rule): RuleInput {
	return {
		channel_id: rule.channel_id,
		post_to: rule.post_to,
		allow_users: rule.allow_users,
		allow_roles: rule.allow_roles,
		enabled: rule.enabled
	};
}

/** Whether a rule only leaves its channel out of a server watched whole. */
export const isExclusion = (rule: Rule): boolean =>
	rule.channel_id !== null && !rule.enabled && !hasOptions(rule);

/**
 * Removes a rule. Removing a server's rule also removes the channel rules that only left
 * channels out of it, since they said nothing else. Returns the ids of every rule removed.
 */
export async function stopWatching(rule: Rule, rules: Rule[]): Promise<Uuid[]> {
	await rulesApi.remove(rule.id);
	const gone = [rule.id];
	if (rule.channel_id === null) {
		const exclusions = rules.filter(
			(r) =>
				r.application_id === rule.application_id && r.guild_id === rule.guild_id && isExclusion(r)
		);
		for (const exclusion of exclusions) {
			await rulesApi.remove(exclusion.id);
			gone.push(exclusion.id);
		}
	}
	return gone;
}

/** Starts watching a server whole: its rule comes back on, or a new one is made. */
export async function watchServer(
	applicationId: Uuid,
	guild: Snowflake,
	serverRule: Rule | null
): Promise<Rule> {
	return serverRule
		? rulesApi.update(serverRule.id, { ...inputOf(serverRule), enabled: true })
		: rulesApi.create(applicationId, guild, { channel_id: null });
}
