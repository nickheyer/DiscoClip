// Turning the settings tree into the flat, typed fields the Settings page edits.

import type { Json, SettingEntry, SettingSource, SettingsView, Timestamp } from './api/types';
import { EMPTY, bytes, clock, number } from './format';

/** What one value is edited as. */
export type ScalarKind = 'boolean' | 'number' | 'string' | 'list' | 'json';
/** A value, or an optional section that is turned on and off as one piece. */
export type FieldKind = ScalarKind | 'section';

/** What a number counts, read off the end of its key. */
export interface Unit {
	/** The word beside the editor, which edits the raw number. */
	label: string;
	/** How the number reads. */
	format: (value: number) => string;
	/** Whether the wording hides the number, so the raw one is worth showing beside it. */
	opaque: boolean;
}

const BYTES: Unit = { label: 'bytes', format: (v) => bytes(v), opaque: true };
const SECONDS: Unit = { label: 'seconds', format: (v) => clock(v), opaque: true };
const MILLISECONDS: Unit = {
	label: 'milliseconds',
	format: (v) => `${number(v)} ms`,
	opaque: false
};
const DAYS: Unit = {
	label: 'days',
	format: (v) => `${number(v)} ${v === 1 ? 'day' : 'days'}`,
	opaque: false
};
const PER_SECOND: Unit = {
	label: 'per second',
	format: (v) => `${number(v)} per second`,
	opaque: false
};

/** The unit the last segment of `key` names, when it names one. */
export function unitOf(key: string): Unit | null {
	const name = key.slice(key.lastIndexOf('.') + 1);
	if (name.endsWith('_bytes')) return BYTES;
	if (name.endsWith('_secs')) return SECONDS;
	if (name.endsWith('_ms')) return MILLISECONDS;
	if (name.endsWith('_days')) return DAYS;
	if (name === 'per_second') return PER_SECOND;
	return null;
}

/** Keys that take one of a fixed set of words. */
const CHOICES: Record<string, string[]> = {
	'engine.archive.keep': ['output', 'source', 'both']
};

/** What is stored for a key: the row at it, or the rows beneath it when it holds a section. */
export interface Stored {
	/** `app` when the app wrote any of it, `provisioning` when the config file wrote all of it. */
	source: SettingSource;
	/** When the newest of them was written. */
	updated_at: Timestamp;
}

/** One value inside an optional section. */
export interface Leaf {
	/** The full dotted key. */
	key: string;
	/** The key inside the section. */
	name: string;
	kind: ScalarKind;
	unit: Unit | null;
	secret: boolean;
	/** Whether a secret is stored, so leaving it blank keeps it. */
	set: boolean;
	/** What a value looks like, from the exemplar. */
	placeholder: string;
	/** The words the key takes, where it takes a fixed set. */
	choices: string[] | null;
}

export interface SettingField {
	/** The dotted key, spelled as the config file spells it. */
	key: string;
	/** The section: the first segment of the key. */
	section: string;
	kind: FieldKind;
	/** The value in force: the stored one, else the default. */
	value: Json;
	default: Json;
	/** The unit the number carries, when the key names one. */
	unit: Unit | null;
	secret: boolean;
	/** What is stored over the default, when anything is. */
	stored: Stored | null;
	/** The values an optional section holds, in key order. Only a section has them. */
	leaves: Leaf[] | null;
}

const isObject = (v: Json): v is Record<string, Json> =>
	typeof v === 'object' && v !== null && !Array.isArray(v);

/** The kind of editor a value takes. */
export function kindOf(value: Json, fallback: Json): ScalarKind {
	const sample = value === null || value === undefined ? fallback : value;
	if (typeof sample === 'boolean') return 'boolean';
	if (typeof sample === 'number') return 'number';
	if (typeof sample === 'string') return 'string';
	if (Array.isArray(sample)) return sample.every((v) => typeof v === 'string') ? 'list' : 'json';
	if (isObject(sample)) return 'json';
	return 'string';
}

/** Whether an object is a map of user-chosen keys rather than a fixed section. */
function isMap(defaults: Json, current: Json): boolean {
	if (!isObject(defaults)) return false;
	if (Object.keys(defaults).length === 0) return true;
	// Fixed sections have the same keys everywhere; a map's own keys are its own.
	if (isObject(current)) {
		const keys = Object.keys(defaults);
		return (
			Object.keys(current).some((k) => !keys.includes(k)) &&
			keys.every((k) => isObject(defaults[k]))
		);
	}
	return false;
}

interface Flat {
	key: string;
	value: Json;
	default: Json;
	exemplar: Json;
}

function walk(prefix: string, defaults: Json, current: Json, exemplar: Json, out: Flat[]): void {
	if (isObject(defaults) && !isMap(defaults, current)) {
		const keys = new Set([
			...Object.keys(defaults),
			...(isObject(current) ? Object.keys(current) : [])
		]);
		for (const key of [...keys].sort()) {
			const child = prefix ? `${prefix}.${key}` : key;
			walk(
				child,
				defaults[key] ?? null,
				isObject(current) ? (current[key] ?? undefined) : undefined,
				isObject(exemplar) ? (exemplar[key] ?? null) : null,
				out
			);
		}
		return;
	}
	out.push({
		key: prefix,
		value: current === undefined ? defaults : current,
		default: defaults,
		exemplar
	});
}

/** The dotted names beneath `value` and what each holds, as the store keeps them. */
function leavesOf(value: Json, prefix: string, out: [string, Json][]): void {
	if (isObject(value) && Object.keys(value).length > 0) {
		for (const key of Object.keys(value).sort()) {
			leavesOf(value[key], prefix ? `${prefix}.${key}` : key, out);
		}
		return;
	}
	out.push([prefix, value]);
}

/** The value at a dotted `name` inside `value`, when there is one. */
export function valueAt(value: Json, name: string): Json | undefined {
	let node: Json | undefined = value;
	for (const segment of name.split('.')) {
		if (!isObject(node)) return undefined;
		node = node[segment];
	}
	return node;
}

/** Puts `leaf` at the dotted `name` inside `target`, making the objects on the way. */
export function setAt(target: Record<string, Json>, name: string, leaf: Json): void {
	const segments = name.split('.');
	let node = target;
	for (const segment of segments.slice(0, -1)) {
		const next = node[segment];
		if (!isObject(next)) node[segment] = {};
		node = node[segment] as Record<string, Json>;
	}
	node[segments[segments.length - 1]] = leaf;
}

/** What a placeholder value reads as inside an input. */
function placeholderText(value: Json): string {
	if (value === null || value === undefined) return '';
	if (Array.isArray(value)) return value.map(String).join(', ');
	if (isObject(value)) return JSON.stringify(value);
	return String(value);
}

/**
 * What the store holds for `key`. A key whose default is absent, such as an optional
 * section, is edited whole while the store holds one row per value beneath it, so the
 * rows beneath the key count as stored at it.
 */
function storedAt(entries: SettingEntry[], key: string): Stored | null {
	const rows = entries.filter((entry) => entry.key === key || entry.key.startsWith(`${key}.`));
	if (rows.length === 0) return null;
	return {
		source: rows.some((entry) => entry.source === 'app') ? 'app' : 'provisioning',
		updated_at: rows.reduce(
			(latest, entry) =>
				Date.parse(entry.updated_at) > Date.parse(latest) ? entry.updated_at : latest,
			rows[0].updated_at
		)
	};
}

/** Every editable field, in key order. */
export function fieldsOf(view: SettingsView): SettingField[] {
	const flat: Flat[] = [];
	walk('', view.defaults, view.settings, view.exemplar, flat);
	return flat.map((f) => {
		const dot = f.key.indexOf('.');
		const section = dot >= 0 ? f.key.slice(0, dot) : f.key;
		const stored = storedAt(view.entries, f.key);
		// An optional section has no default. The exemplar shows the values it takes.
		if (f.default === null && isObject(f.exemplar) && !isMap(f.exemplar, f.value)) {
			const shape: [string, Json][] = [];
			leavesOf(f.exemplar, '', shape);
			const leaves = shape.map(([name, sample]): Leaf => {
				const key = `${f.key}.${name}`;
				const secret = view.secret_keys.includes(key);
				return {
					key,
					name,
					kind: kindOf(sample, sample),
					unit: unitOf(key),
					secret,
					set: secret && view.secrets.includes(key),
					placeholder: secret ? '' : placeholderText(sample),
					choices: CHOICES[key] ?? null
				};
			});
			return {
				key: f.key,
				section,
				kind: 'section',
				value: f.value,
				default: null,
				unit: null,
				secret: false,
				stored,
				leaves
			};
		}
		return {
			key: f.key,
			section,
			kind: kindOf(f.value, f.default ?? f.exemplar),
			value: f.value,
			default: f.default,
			unit: unitOf(f.key),
			secret: view.secret_keys.includes(f.key),
			stored,
			leaves: null
		};
	});
}

/** How a value reads where someone looks at it rather than edits it. */
export function display(value: Json, kind: FieldKind, unit: Unit | null = null): string {
	if (value === null || value === undefined) return EMPTY;
	switch (kind) {
		case 'boolean':
			return value ? 'On' : 'Off';
		case 'number':
			return unit && typeof value === 'number' ? unit.format(value) : String(value);
		case 'string':
			return String(value);
		case 'list':
			return Array.isArray(value) && value.length > 0 ? value.map(String).join(', ') : EMPTY;
		case 'json':
		case 'section':
			return JSON.stringify(value);
	}
}

/** An optional section in one line: each value it holds, secrets hidden. */
export function summary(field: SettingField): string {
	if (field.value === null || field.value === undefined || !field.leaves) return EMPTY;
	return field.leaves
		.map((leaf) => {
			const held = leaf.secret
				? leaf.set
					? '••••••••'
					: EMPTY
				: display(valueAt(field.value, leaf.name) ?? null, leaf.kind, leaf.unit);
			return `${leaf.name}: ${held}`;
		})
		.join(' · ');
}

/** The number behind a formatted value, where the unit's wording hides it. */
export function rawNumber(value: Json, unit: Unit | null): string | null {
	if (!unit || !unit.opaque || typeof value !== 'number') return null;
	return String(value);
}
