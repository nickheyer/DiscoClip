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
	'rule.delete': 'Rule removed'
};

export function actionTone(
	action: Action
): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent' {
	if (action.endsWith('.create') || action === 'bot.start') return 'ok';
	if (action.endsWith('.delete') || action === 'bot.stop') return 'danger';
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
		default:
			return null;
	}
}

export const TARGET_KIND_LABELS = {
	setting: 'Setting',
	application: 'Application',
	rule: 'Rule'
} as const;
