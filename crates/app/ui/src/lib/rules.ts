import type { Rule, RuleInput } from './api/types';
import { formatBytes, formatDuration } from './format';

export function emptyRule(): RuleInput {
	return {
		channel_id: '',
		post_to: null,
		allow_hosts: [],
		allow_users: [],
		allow_roles: [],
		max_source_bytes: null,
		max_duration_secs: null,
		max_height: null,
		enabled: true
	};
}

/** What a stored rule says, ready to be sent back changed. */
export function toInput(rule: Rule): RuleInput {
	return {
		channel_id: rule.channel_id,
		post_to: rule.post_to,
		allow_hosts: [...rule.allow_hosts],
		allow_users: [...rule.allow_users],
		allow_roles: [...rule.allow_roles],
		max_source_bytes: rule.max_source_bytes,
		max_duration_secs: rule.max_duration_secs,
		max_height: rule.max_height,
		enabled: rule.enabled
	};
}

/** The input with every string trimmed and empty optionals as `null`. */
export function cleanInput(input: RuleInput): RuleInput {
	return {
		channel_id: input.channel_id.trim(),
		post_to: input.post_to?.trim() || null,
		allow_hosts: (input.allow_hosts ?? []).map((h) => h.trim()).filter(Boolean),
		allow_users: (input.allow_users ?? []).map((u) => u.trim()).filter(Boolean),
		allow_roles: (input.allow_roles ?? []).map((r) => r.trim()).filter(Boolean),
		max_source_bytes: input.max_source_bytes ?? null,
		max_duration_secs: input.max_duration_secs ?? null,
		max_height: input.max_height ?? null,
		enabled: input.enabled ?? true
	};
}

export function describeFilters(rule: RuleInput): string {
	const parts: string[] = [];
	const hosts = rule.allow_hosts ?? [];
	const users = rule.allow_users ?? [];
	const roles = rule.allow_roles ?? [];
	if (hosts.length) parts.push(hosts.length === 1 ? hosts[0]! : `${hosts.length} hosts`);
	if (users.length) parts.push(`${users.length} user${users.length === 1 ? '' : 's'}`);
	if (roles.length) parts.push(`${roles.length} role${roles.length === 1 ? '' : 's'}`);
	return parts.length ? parts.join(' · ') : 'Everyone, every host';
}

export function describeLimits(rule: RuleInput): string {
	const parts: string[] = [];
	if (rule.max_source_bytes != null) parts.push(`≤ ${formatBytes(rule.max_source_bytes)}`);
	if (rule.max_duration_secs != null) parts.push(`≤ ${formatDuration(rule.max_duration_secs)}`);
	if (rule.max_height != null) parts.push(`≤ ${rule.max_height}p`);
	return parts.length ? parts.join(' · ') : 'Server limits';
}

export function sameInput(a: RuleInput, b: RuleInput): boolean {
	return JSON.stringify(cleanInput(a)) === JSON.stringify(cleanInput(b));
}
