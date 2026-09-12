import type { Permission, Role } from './api/types';

/** The roles table in API.md. */
export const ROLE_PERMISSIONS: Record<Role, readonly Permission[]> = {
	admin: [
		'manage_users',
		'manage_applications',
		'view_audit_log',
		'manage_settings',
		'manage_watch_rules',
		'manage_bots',
		'manage_jobs'
	],
	operator: ['manage_watch_rules', 'manage_bots', 'manage_jobs'],
	viewer: []
};

export function roleAllows(role: Role, permission: Permission): boolean {
	return ROLE_PERMISSIONS[role].includes(permission);
}

export const ROLE_LABELS: Record<Role, { label: string; description: string }> = {
	admin: {
		label: 'Admin',
		description:
			'Everything: accounts, Discord applications, settings, rules, bots and the audit log.'
	},
	operator: {
		label: 'Operator',
		description: 'Jobs, watch rules and the bots.'
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
	},
	manage_jobs: {
		label: 'Manage jobs',
		description: 'Submit links, and retry, cancel and delete jobs.'
	},
	manage_settings: {
		label: 'Manage settings',
		description: 'Read and change the server’s settings, and import and export them.'
	}
};
