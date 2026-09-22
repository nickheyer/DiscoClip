<script lang="ts">
	import { untrack } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import Alert from './Alert.svelte';
	import Badge from './Badge.svelte';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import SettingField from './SettingField.svelte';
	import SettingMap from './SettingMap.svelte';
	import FormFeedback from './FormFeedback.svelte';
	import type { SettingValue, SettingsChange, SettingsView } from '$lib/api';
	import { isBlank, problemOf, sourceOf } from '$lib/settings/model';
	import { at, sameValue, type SectionSpec } from '$lib/settings/schema';

	interface Props {
		spec: SectionSpec;
		view: SettingsView;
		/** Saves the change and resolves once the view has been reloaded. */
		onsave: (change: SettingsChange) => Promise<void>;
		/** Setting key to highlight. */
		highlight?: string | null;
		/** Report unsaved changes. */
		ondirty?: (dirty: boolean) => void;
	}

	let { spec, view, onsave, highlight = null, ondirty }: Props = $props();

	const effectiveSection = $derived(at(view.settings, spec.key));
	const sectionOn = $derived(!spec.optional || (effectiveSection !== null && effectiveSection !== undefined));
	const sectionSource = $derived(sourceOf(view, spec.key));

	// The draft: one value per field key, plus whether an optional section is on.
	let draft = $state<Record<string, SettingValue | undefined>>({});
	let enabled = $state(true);
	let resets = new SvelteSet<string>();
	let base: Record<string, SettingValue | undefined> = {};
	let baseEnabled = true;
	let versions = $state<Record<string, number>>({});
	let saving = $state(false);
	let error = $state<string | null>(null);
	let submitted = $state(false);
	let maps = $state<Record<string, SettingMap | undefined>>({});

	function fieldKey(name: string): string {
		return `${spec.key}.${name}`;
	}

	function fresh(): Record<string, SettingValue | undefined> {
		const next: Record<string, SettingValue | undefined> = {};
		for (const field of spec.fields) {
			const key = fieldKey(field.name);
			const current = at(view.settings, key);
			next[key] =
				field.kind === 'secret'
					? undefined
					: current !== undefined
						? current
						: sectionOn
							? undefined
							: at(view.defaults, key);
		}
		for (const map of spec.maps ?? []) {
			next[map.key] = at(view.settings, map.key) ?? {};
		}
		return next;
	}

	function bump(key: string) {
		versions[key] = (versions[key] ?? 0) + 1;
	}

	/** Restore the saved values. */
	function reload() {
		base = fresh();
		baseEnabled = sectionOn;
		draft = { ...base };
		enabled = baseEnabled;
		resets.clear();
		error = null;
		submitted = false;
		for (const key of Object.keys(base)) bump(key);
	}

	function rebase() {
		const next = fresh();
		for (const key of Object.keys(next)) {
			const edited = resets.has(key) || !sameValue(draft[key], base[key]);
			if (!edited && !sameValue(draft[key], next[key])) {
				draft[key] = next[key];
				bump(key);
			}
		}
		if (enabled === baseEnabled) enabled = sectionOn;
		base = next;
		baseEnabled = sectionOn;
	}
	// Watch only view. Draft writes must not trigger another rebase.
	$effect.pre(() => {
		void view;
		untrack(rebase);
	});

	function useDefault(key: string) {
		draft[key] = at(view.defaults, key) ?? null;
		resets.add(key);
		bump(key);
	}

	/** Whether `key` was sent back to its default and left there. */
	function resetting(key: string): boolean {
		return resets.has(key) && sameValue(draft[key], at(view.defaults, key) ?? null);
	}

	const fieldProblems = $derived.by(() => {
		if (spec.optional && !enabled) return [];
		return spec.fields
			.map((field) => {
				const key = fieldKey(field.name);
				const value = draft[key];
				if (field.kind === 'secret') {
					const set = view.secrets.includes(key) && sectionOn;
					return set || !isBlank(value) ? null : 'Enter a value.';
				}
				return problemOf(field, value);
			})
			.filter((p): p is string => p !== null);
	});
	const mapsValid = $derived((spec.maps ?? []).every((map) => maps[map.key]?.valid() ?? true));

	/** Pending API changes. */
	const change = $derived.by((): SettingsChange => {
		const set: Record<string, SettingValue> = {};
		const reset: string[] = [];
		if (spec.optional) {
			if (!enabled) {
				if (sectionOn) set[spec.key] = null;
				return { set, reset };
			}
			if (!sectionOn) {
				const whole: Record<string, SettingValue> = {};
				for (const field of spec.fields) {
					const value = draft[fieldKey(field.name)];
					if (value !== undefined) whole[field.name] = value;
				}
				set[spec.key] = whole;
				return { set, reset };
			}
		}
		for (const field of spec.fields) {
			const key = fieldKey(field.name);
			const value = draft[key];
			if (resetting(key)) {
				if (view.entries.some((e) => e.key === key || e.key.startsWith(`${key}.`))) reset.push(key);
				continue;
			}
			if (field.kind === 'secret') {
				if (!isBlank(value)) set[key] = value as SettingValue;
				continue;
			}
			if (value === undefined) continue;
			if (!sameValue(value, at(view.settings, key))) set[key] = value;
		}
		for (const map of spec.maps ?? []) {
			const value = draft[map.key];
			if (resetting(map.key)) {
				if (view.entries.some((e) => e.key === map.key)) reset.push(map.key);
				continue;
			}
			if (value !== undefined && !sameValue(value, at(view.settings, map.key) ?? {})) {
				set[map.key] = value;
			}
		}
		return { set, reset };
	});
	const dirty = $derived(
		Object.keys(change.set ?? {}).length > 0 || (change.reset?.length ?? 0) > 0
	);
	const ready = $derived(dirty && fieldProblems.length === 0 && mapsValid);

	$effect(() => {
		ondirty?.(dirty);
	});
	const highlighted = $derived(
		highlight !== null && (highlight === spec.key || highlight.startsWith(`${spec.key}.`))
	);

	async function save(event: SubmitEvent) {
		event.preventDefault();
		submitted = true;
		if (!ready) {
			error = 'Check the fields below.';
			return;
		}
		saving = true;
		error = null;
		try {
			await onsave(change);
			reload();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : String(cause);
		} finally {
			saving = false;
		}
	}

	async function resetSection() {
		saving = true;
		error = null;
		try {
			await onsave({ reset: [spec.key] });
			reload();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : String(cause);
		} finally {
			saving = false;
		}
	}

	const stored = $derived(
		view.entries.some((e) => e.key === spec.key || e.key.startsWith(`${spec.key}.`))
	);
</script>

<form class={['card', highlighted && 'highlighted']} id={`section-${spec.key}`} onsubmit={save} novalidate tabindex="-1">
	<div class="card-header">
		<div class="title">
			<div>
				<h2>{spec.title}</h2>
				<p class="hint">{spec.description}</p>
			</div>
		</div>
		<div class="row">
			{#if spec.optional}
				{#if sectionOn}
					<Badge tone={sectionSource.source === 'provisioning' ? 'info' : 'ok'} size="sm" dot>On</Badge>
				{:else}
					<Badge size="sm">Off</Badge>
				{/if}
			{/if}
		</div>
	</div>
	<div class="card-body">
		{#if error}
			<div class="error-box"><FormFeedback message={error} /></div>
		{/if}
		{#if spec.optional}
			<label class="checkbox toggle">
				<input type="checkbox" bind:checked={enabled} disabled={saving} />
				<span>
					<span class="strong">{spec.optional.label}</span>
					{#if spec.optional.hint}<span class="hint">{spec.optional.hint}</span>{/if}
				</span>
			</label>
		{/if}
		{#if !spec.optional || enabled}
			{#each spec.fields as field (field.name)}
				{@const key = fieldKey(field.name)}
				{@const source = sourceOf(view, key)}
				{#key versions[key] ?? 0}
					<SettingField
						id={key}
						spec={field}
						bind:value={draft[key]}
						effective={at(view.settings, key)}
						fallback={at(view.defaults, key)}
						source={source.source}
						updatedAt={source.entry?.updated_at ?? null}
						secretSet={view.secrets.includes(key) && sectionOn}
						disabled={saving}
						showErrors={submitted}
						dormant={spec.optional !== undefined && !sectionOn}
						onreset={() => useDefault(key)}
					/>
				{/key}
			{/each}
			{#each spec.maps ?? [] as map (map.key)}
				{@const source = sourceOf(view, map.key)}
				{#key versions[map.key] ?? 0}
					<SettingMap
						spec={map}
						bind:value={draft[map.key]}
						bind:this={maps[map.key]}
						effective={at(view.settings, map.key)}
						source={source.source}
						updatedAt={source.entry?.updated_at ?? null}
						disabled={saving}
						onreset={() => useDefault(map.key)}
					/>
				{/key}
			{/each}
		{/if}
		{#if spec.fields.length === 0 && (spec.maps ?? []).length === 0}
			<p class="faint small">Choose a subsection from the menu.</p>
		{/if}
	</div>
	<div class="card-footer">
		{#if spec.optional && stored}
			<Button variant="ghost" onclick={resetSection} disabled={saving}>Restore defaults</Button>
		{/if}
		<span class="grow"></span>
		{#if dirty}<span class="hint" role="status">Unsaved changes</span>{/if}
		<Button variant="ghost" onclick={reload} disabled={!dirty || saving}>Discard changes</Button>
		<Button type="submit" variant="primary" loading={saving} disabled={!dirty}>Save changes</Button>
	</div>
</form>

<style>
	/* Jumped to from the contents: clear of the window's edge, and of the top bar on a phone. */
	form {
		scroll-margin-top: 20px;
	}

	@media (max-width: 900px) {
		form {
			scroll-margin-top: 64px;
		}
	}

	.title {
		display: flex;
		align-items: flex-start;
		gap: 12px;
		min-width: 0;
	}

	.toggle {
		padding: 4px 0 20px;
		border-bottom: 1px solid var(--border);
		margin-bottom: 4px;
	}

	.error-box {
		margin-bottom: 12px;
	}

	.grow {
		flex: 1;
	}

	.card-footer {
		justify-content: flex-start;
		position: sticky;
		bottom: 0;
		z-index: 3;
		flex-wrap: wrap;
	}

	.highlighted {
		border-color: var(--accent);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
	}
</style>
