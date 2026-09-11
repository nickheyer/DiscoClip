import type { Permission, Role } from './api/types';

/** The roles table in API.md. */
export const ROLE_PERMISSIONS: Record<Role, readonly Permission[]> = {
	admin: [
		'manage_users',
		'manage_applications',
		'view_audit_log',
		'manage_watch_rules',
		'manage_bots'
	],
	operator: ['manage_watch_rules', 'manage_bots'],
	viewer: []
};

export function roleAllows(role: Role, permission: Permission): boolean {
	return ROLE_PERMISSIONS[role].includes(permission);
}

export const ROLE_LABELS: Record<Role, { label: string; description: string }> = {
	admin: {
		label: 'Admin',
		description: 'Everything: accounts, Discord applications, rules, bots and the audit log.'
	},
	operator: {
		label: 'Operator',
		description: 'Watch rules and the bots.'
	},
	viewer: {
		label: 'Viewer',
		description: 'Read only, plus the rules of guilds they manage on Discord.'
	}
};

export const PERMISSION_LABELS: Record<Permission, { label: string; description: string }> = {
	manage_users: {
		label: 'Manage accounts',
		description: 'Create accounts, assign roles, reset passwords and end anyone’s sessions.'
	},
	manage_applications: {
		label: 'Manage applications',
		description: 'Add, change and remove the Discord applications the server runs bots for.'
	},
	manage_watch_rules: {
		label: 'Manage watch rules',
		description: 'Edit the watch rules of any guild.'
	},
	manage_bots: {
		label: 'Run the bots',
		description: 'Start, stop and restart the bots.'
	},
	view_audit_log: {
		label: 'View the audit log',
		description: 'Read who changed which setting, application, rule or bot.'
	}
};
