// What the front end pages say about front ends: labels, summaries and an empty form.

import type {
	Frontend,
	FrontendInput,
	FrontCallbackError,
	FrontProvider,
	MediaKind,
	SecretKind
} from './api/types';
import { formatBytes, pluralize } from './format';

export const SECRET_LABELS: Record<SecretKind, { label: string; prompt: string; hint: string }> = {
	pin: { label: 'PIN', prompt: 'Enter the PIN', hint: 'A short code of digits to remember.' },
	password: {
		label: 'Password',
		prompt: 'Enter the password',
		hint: 'A password shared with everyone who may look.'
	},
	token: {
		label: 'Access token',
		prompt: 'Paste the access token',
		hint: 'A long string handed out and pasted rather than remembered.'
	}
};

export const FRONT_CALLBACK_ERRORS: Record<FrontCallbackError, string> = {
	state: 'That login attempt expired or was already used. Start again.',
	denied: 'You cancelled at the provider, so nothing was changed.',
	provider: 'The provider refused the request, or this site does not log people in through it.',
	exchange: 'The provider could not be reached to finish the login.',
	identity: 'The provider did not send back a usable identity.',
	frontend: 'This site is not taking logins right now.',
	not_listed: 'Your Discord account is not on the list of people who may look.',
	not_member: 'You are not a member of the Discord server this site belongs to.',
	guilds: 'Discord did not say which servers you belong to. Try again.'
};

export function frontCallbackMessage(code: string | null): string | null {
	if (!code) return null;
	if (code in FRONT_CALLBACK_ERRORS) return FRONT_CALLBACK_ERRORS[code as FrontCallbackError];
	return `The provider login failed (${code}).`;
}

export const MEDIA_ICONS: Record<MediaKind, string> = {
	video: 'video',
	audio: 'volume',
	image: 'img',
	file: 'file-text'
};

export function emptyFrontend(profile: string): FrontendInput {
	return {
		name: '',
		slug: '',
		description: '',
		enabled: true,
		profile_id: profile,
		scope: { guilds: [], channels: [] },
		access: {
			open: false,
			secret_kind: null,
			accounts: false,
			providers: [],
			discord_members: false,
			discord_users: []
		},
		downloads: true,
		links: {
			enabled: false,
			min_height: 720,
			min_bitrate: 1_500_000,
			max_bytes: 2 * 1024 * 1024 * 1024,
			signed_link_days: 30
		}
	};
}

export function toInput(frontend: Frontend): FrontendInput {
	return {
		name: frontend.name,
		slug: frontend.slug,
		description: frontend.description,
		enabled: frontend.enabled,
		profile_id: frontend.profile_id,
		scope: { guilds: [...frontend.scope.guilds], channels: [...frontend.scope.channels] },
		access: {
			open: frontend.access.open,
			secret_kind: frontend.access.secret_kind,
			accounts: frontend.access.accounts,
			providers: [...frontend.access.providers],
			discord_members: frontend.access.discord_members,
			discord_users: [...frontend.access.discord_users]
		},
		downloads: frontend.downloads,
		links: { ...frontend.links }
	};
}

/** Which jobs the front end shows, in words. */
export function scopeSummary(frontend: Pick<Frontend, 'scope'>): string {
	const parts: string[] = [];
	if (frontend.scope.guilds.length) parts.push(pluralize(frontend.scope.guilds.length, 'guild'));
	if (frontend.scope.channels.length)
		parts.push(pluralize(frontend.scope.channels.length, 'channel'));
	return parts.length ? parts.join(' and ') : 'everything';
}

/** Who the front end lets in, in words. */
export function accessSummary(
	frontend: Pick<Frontend, 'access' | 'has_secret'>,
	providers: FrontProvider[] = []
): string {
	if (frontend.access.open) return 'anyone';
	const ways: string[] = [];
	if (frontend.has_secret && frontend.access.secret_kind)
		ways.push(SECRET_LABELS[frontend.access.secret_kind].label.toLowerCase());
	if (frontend.access.accounts) ways.push('own accounts');
	for (const id of frontend.access.providers) {
		const name = providers.find((p) => p.id === id)?.name ?? id;
		ways.push(
			id === 'discord' && frontend.access.discord_members ? `${name} members` : name
		);
	}
	return ways.length ? ways.join(', ') : 'nobody: no way in is set up';
}

export function linkSummary(frontend: Pick<Frontend, 'links'>): string {
	if (!frontend.links.enabled) return 'uploads only';
	return `links under ${frontend.links.min_height}p or ${Math.round(frontend.links.min_bitrate / 1000)} kb/s, page up to ${formatBytes(frontend.links.max_bytes)}`;
}

/** The slug rule the server applies, so the form can say so before sending. */
export function slugProblem(slug: string): string | null {
	if (slug.length < 2 || slug.length > 40) return 'A slug is 2 to 40 characters.';
	if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(slug))
		return 'Lower-case letters, digits and dashes, not starting or ending with a dash.';
	if (['api', 'login', 'logout', 'setup', 'static', '_app'].includes(slug))
		return `${slug} is reserved.`;
	return null;
}
