import type { Action, Target } from './api/types';

export const ACTION_LABELS: Record<Action, string> = {
	'settings.set': 'Setting changed',
	'settings.reset': 'Setting reset',
	'settings.import': 'Settings imported',
	'settings.provision': 'Setting provisioned',
	'application.create': 'Application added',
	'application.update': 'Application changed',
	'application.delete': 'Application removed',
	'application.commands.set': 'Command scope set',
	'application.commands.register': 'Commands registered',
	'bot.start': 'Bot started',
	'bot.stop': 'Bot stopped',
	'bot.restart': 'Bot restarted',
	'rule.create': 'Rule added',
	'rule.update': 'Rule changed',
	'rule.delete': 'Rule removed',
	'session.import': 'Session cookies imported',
	'session.clear': 'Session cookies cleared',
	'profile.create': 'Profile added',
	'profile.update': 'Profile changed',
	'profile.delete': 'Profile removed',
	'profile.assign': 'Profile put assigned',
	'profile.unassign': 'Profile taken off',
	'frontend.create': 'Media site added',
	'frontend.update': 'Media site changed',
	'frontend.delete': 'Media site removed',
	'frontend.secret.set': 'Media site secret set',
	'frontend.secret.clear': 'Media site secret cleared',
	'frontend.user.create': 'Media site account added',
	'frontend.user.password': 'Media site account password reset',
	'frontend.user.delete': 'Media site account removed',
	'frontend.sessions.revoke': 'Media site sessions ended'
};

export function actionTone(
	action: Action
): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent' {
	if (action.endsWith('.create') || action === 'bot.start') return 'ok';
	if (
		action.endsWith('.delete') ||
		action === 'bot.stop' ||
		action === 'session.clear' ||
		action === 'frontend.secret.clear' ||
		action === 'frontend.sessions.revoke'
	)
		return 'danger';
	if (action === 'session.import' || action === 'profile.assign' || action === 'frontend.secret.set')
		return 'accent';
	if (action === 'profile.unassign' || action === 'frontend.user.password') return 'warn';
	if (action === 'bot.restart' || action === 'application.commands.register') return 'info';
	if (action.startsWith('settings.')) return 'accent';
	return 'neutral';
}

export function targetHref(target: Target): string | null {
	switch (target.kind) {
		case 'application':
			return `/applications/${encodeURIComponent(target.id)}`;
		case 'rule':
			return `/rules/${encodeURIComponent(target.id)}`;
		case 'setting':
			return `/settings?key=${encodeURIComponent(target.id)}`;
		case 'platform':
			return '/platforms';
		case 'profile':
			return `/profiles/${encodeURIComponent(target.id)}`;
		case 'frontend':
			return '/frontends';
		default:
			return null;
	}
}

export const TARGET_KIND_LABELS = {
	setting: 'Setting',
	application: 'Application',
	rule: 'Rule',
	platform: 'Platform',
	profile: 'Profile',
	frontend: 'Media site'
} as const;

/** The account a media site account entry names, when its details carry one. */
export function auditUsername(details: Record<string, unknown> | null | undefined): string | null {
	const username = details?.username;
	return typeof username === 'string' && username ? username : null;
}

/** The scope an assignment entry names, in words, when its details carry one. */
export function auditScope(details: Record<string, unknown> | null | undefined): string | null {
	const scope = details?.scope;
	if (!scope || typeof scope !== 'object') return null;
	const s = scope as { kind?: string; guild_id?: string; channel_id?: string; user_id?: string };
	switch (s.kind) {
		case 'global':
			return 'the whole server';
		case 'guild':
			return `server ${s.guild_id}`;
		case 'channel':
			return `channel ${s.channel_id} of server ${s.guild_id}`;
		case 'user':
			return `user ${s.user_id} in server ${s.guild_id}`;
		default:
			return null;
	}
}
