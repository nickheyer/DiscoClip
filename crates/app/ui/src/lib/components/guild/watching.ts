// What the watch switches do: a rule per channel switched on, a rule without a channel for
// a server watched whole, and under that, a channel rule switched off to leave a channel out.

import { rules as rulesApi } from '$lib/api/endpoints';
import type { Rule, RuleInput, Snowflake, Uuid } from '$lib/api/types';

/** The rule as it would be sent back, to change one field of it. */
export function inputOf(rule: Rule): RuleInput {
	return { channel_id: rule.channel_id, enabled: rule.enabled };
}

/** Whether a rule only leaves its channel out of a server watched whole. */
export const isExclusion = (rule: Rule): boolean => rule.channel_id !== null && !rule.enabled;

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
