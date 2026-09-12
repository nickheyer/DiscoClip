// How the settings page reads a settings view and turns edits into a change.

import type { SettingEntry, SettingSource, SettingValue, SettingsView } from '$lib/api';
import type { FieldKind, FieldSpec } from './schema';

export type ValueSource = SettingSource | 'default';

/** Who set the value at `key`: the stored row at or above it, else the default. */
export function sourceOf(
	view: SettingsView,
	key: string
): { source: ValueSource; entry: SettingEntry | null } {
	const exact = view.entries.find((e) => e.key === key);
	if (exact) return { source: exact.source, entry: exact };
	const beneath = view.entries.filter((e) => e.key.startsWith(`${key}.`));
	if (beneath.length) {
		// A section: provisioned when every row beneath it is, set in the app otherwise.
		const source: ValueSource = beneath.every((e) => e.source === 'provisioning')
			? 'provisioning'
			: 'app';
		const latest = beneath.reduce((a, b) => (a.updated_at > b.updated_at ? a : b));
		return { source, entry: latest };
	}
	const above = view.entries.find((e) => key.startsWith(`${e.key}.`));
	if (above) return { source: above.source, entry: above };
	return { source: 'default', entry: null };
}

export const SOURCE_LABELS: Record<ValueSource, string> = {
	app: 'Set here',
	provisioning: 'Provisioned',
	default: 'Default'
};

export const BYTE_UNITS = [
	{ label: 'MB', factor: 1024 ** 2 },
	{ label: 'GB', factor: 1024 ** 3 }
];

export const TIME_UNITS = [
	{ label: 'seconds', factor: 1 },
	{ label: 'minutes', factor: 60 },
	{ label: 'hours', factor: 3600 }
];

/** The unit a stored amount is best shown in: the largest that divides it. */
export function unitFor(value: number, units: { label: string; factor: number }[]): string {
	const fitting = [...units].reverse().find((u) => value >= u.factor && value % u.factor === 0);
	return (fitting ?? units[0]!).label;
}

export function isBlank(value: SettingValue | undefined): boolean {
	return value === undefined || value === null || value === '';
}

/** A message when `value` does not fit `spec`; nothing when it does. */
export function problemOf(spec: FieldSpec, value: SettingValue | undefined): string | null {
	if (isBlank(value)) {
		if (spec.nullable) return null;
		if (spec.kind === 'list') return null;
		if (spec.kind === 'secret') return null;
		return 'A value is needed.';
	}
	switch (spec.kind) {
		case 'integer':
		case 'bytes':
		case 'seconds':
		case 'days':
		case 'millis': {
			if (typeof value !== 'number' || !Number.isInteger(value)) return 'A whole number.';
			if (spec.min !== undefined && value < spec.min) return `At least ${spec.min}.`;
			return null;
		}
		case 'number': {
			if (typeof value !== 'number' || !Number.isFinite(value)) return 'A number.';
			if (spec.min !== undefined && value < spec.min) return `At least ${spec.min}.`;
			return null;
		}
		case 'boolean':
			return typeof value === 'boolean' ? null : 'Yes or no.';
		case 'url': {
			if (typeof value !== 'string') return 'A URL.';
			try {
				const url = new URL(value);
				return url.protocol ? null : 'A URL with a scheme.';
			} catch {
				return 'A URL such as https://example.com.';
			}
		}
		case 'socket':
			return typeof value === 'string' &&
				/^(\[[0-9a-f:.]+\]|[^:\s[\]]+):\d{1,5}$/i.test(value.trim())
				? null
				: 'An address and port such as 127.0.0.1:8080 or [::]:8080.';
		case 'list':
			return Array.isArray(value) ? null : 'A list.';
		case 'enum':
			return spec.options?.some((o) => o.value === value) ? null : 'One of the choices.';
		default:
			return typeof value === 'string' ? null : 'Text.';
	}
}

export function kindIsNumber(kind: FieldKind): boolean {
	return (
		kind === 'integer' ||
		kind === 'number' ||
		kind === 'bytes' ||
		kind === 'seconds' ||
		kind === 'days' ||
		kind === 'millis'
	);
}
