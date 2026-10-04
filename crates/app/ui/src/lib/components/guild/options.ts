// A place's posting options, read from the profile at its scope and written back as its overlay

import type {
	Assignment,
	EffectivePolicy,
	GuildChannel,
	Profile,
	Scope,
	Snowflake
} from '$lib/api/types';
import { scopeKey } from '$lib/api/endpoints';
import { countText } from '$lib/format';
import { channelLabel } from './channels';

/** Where results go, left to the wider scope, the channel the link was seen in, or a channel */
export type Destination = 'inherit' | 'same' | Snowflake;

/** Whose links count, left to the wider scope, everyone, or the members and roles listed */
export type Posters =
	| { kind: 'inherit' }
	| { kind: 'everyone' }
	| { kind: 'chosen'; users: Snowflake[]; roles: Snowflake[] };

export interface Options {
	destination: Destination;
	posters: Posters;
}

/** The options a profile names, each left to the wider scope where it names none */
export function optionsOf(profile: Profile | null): Options {
	const message = profile?.message ?? {};
	const intake = profile?.intake ?? {};
	const users = intake.allow_users ?? null;
	const roles = intake.allow_roles ?? null;
	return {
		destination: !('destination' in message) ? 'inherit' : (message.destination ?? 'same'),
		posters:
			users === null && roles === null
				? { kind: 'inherit' }
				: (users?.length ?? 0) + (roles?.length ?? 0) === 0
					? { kind: 'everyone' }
					: { kind: 'chosen', users: users ?? [], roles: roles ?? [] }
	};
}

/** The options in force at a place, every value settled */
export function settledOptions(effective: EffectivePolicy): Options {
	const { allow_users, allow_roles } = effective.intake;
	return {
		destination: effective.message.destination ?? 'same',
		posters:
			allow_users.length + allow_roles.length === 0
				? { kind: 'everyone' }
				: { kind: 'chosen', users: allow_users, roles: allow_roles }
	};
}

/** The profile assigned at the scope, when the list has it */
export function profileAt(
	scope: Scope,
	assignments: Assignment[],
	profiles: Profile[]
): Profile | null {
	const key = scopeKey(scope);
	const assignment = assignments.find((a) => scopeKey(a.scope) === key);
	return assignment ? (profiles.find((p) => p.id === assignment.profile_id) ?? null) : null;
}

/** Where results go, in a word or a channel's name */
export function destinationLabel(destination: Destination, channels: GuildChannel[]): string {
	if (destination === 'inherit') return 'Inherit';
	if (destination === 'same') return 'Same channel';
	return channelLabel(
		channels.find((c) => c.id === destination),
		destination
	);
}

/** Whose links count, in a word or a count of members and roles */
export function postersLabel(posters: Posters): string {
	switch (posters.kind) {
		case 'inherit':
			return 'Inherit';
		case 'everyone':
			return 'Everyone';
		case 'chosen':
			return [
				posters.users.length > 0 ? countText(posters.users.length, 'member') : '',
				posters.roles.length > 0 ? countText(posters.roles.length, 'role') : ''
			]
				.filter(Boolean)
				.join(' · ');
	}
}

/** The options a place names in a few words, nothing while it inherits both */
export function optionsSummary(options: Options, channels: GuildChannel[]): string[] {
	const parts: string[] = [];
	if (options.destination !== 'inherit')
		parts.push(`to ${destinationLabel(options.destination, channels)}`);
	if (options.posters.kind !== 'inherit') parts.push(postersLabel(options.posters));
	return parts;
}
