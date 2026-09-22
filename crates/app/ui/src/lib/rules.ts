import type { Rule, RuleInput } from './api/types';

export function emptyRule(): RuleInput {
	return {
		channel_id: '',
		post_to: null,
		allow_users: [],
		allow_roles: [],
		enabled: true
	};
}

/** What a stored rule says, ready to be sent back changed. */
export function toInput(rule: Rule): RuleInput {
	return {
		channel_id: rule.channel_id,
		post_to: rule.post_to,
		allow_users: [...rule.allow_users],
		allow_roles: [...rule.allow_roles],
		enabled: rule.enabled
	};
}

/** The input with every string trimmed and empty optionals as `null`. */
export function cleanInput(input: RuleInput): RuleInput {
	return {
		channel_id: input.channel_id.trim(),
		post_to: input.post_to?.trim() || null,
		allow_users: (input.allow_users ?? []).map((u) => u.trim()).filter(Boolean),
		allow_roles: (input.allow_roles ?? []).map((r) => r.trim()).filter(Boolean),
		enabled: input.enabled ?? true
	};
}

/** Who may post links under a rule, in a few words. */
export function describeWho(rule: RuleInput): string {
	const parts: string[] = [];
	const users = rule.allow_users ?? [];
	const roles = rule.allow_roles ?? [];
	if (users.length) parts.push(`${users.length} member${users.length === 1 ? '' : 's'}`);
	if (roles.length) parts.push(`${roles.length} role${roles.length === 1 ? '' : 's'}`);
	return parts.length ? parts.join(' · ') : 'Everyone';
}

export function sameInput(a: RuleInput, b: RuleInput): boolean {
	return JSON.stringify(cleanInput(a)) === JSON.stringify(cleanInput(b));
}
