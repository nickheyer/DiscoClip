<script lang="ts">
	import { untrack } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import Alert from './Alert.svelte';
	import Badge from './Badge.svelte';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import SettingField from './SettingField.svelte';
	import SettingMap from './SettingMap.svelte';
	import type { SettingValue, SettingsChange, SettingsView } from '$lib/api';
	import { isBlank, problemOf, sourceOf } from '$lib/settings/model';
	import { at, sameValue, type SectionSpec } from '$lib/settings/schema';

	interface Props {
		spec: SectionSpec;
		view: SettingsView;
		/** Saves the change and resolves once the view has been reloaded. */
		onsave: (change: SettingsChange) => Promise<void>;
		/** The key the address named, to draw the eye to. */
		highlight?: string | null;
	}

	let { spec, view, onsave, highlight = null }: Props = $props();

	const effectiveSection = $derived(at(view.settings, spec.key));
	const sectionOn = $derived(!spec.optional || (effectiveSection !== null && effectiveSection !== undefined));
	const sectionSource = $derived(sourceOf(view, spec.key));

	// The draft: one value per field key, plus whether an optional section is on.
	let draft = $state<Record<string, SettingValue | undefined>>({});
	let enabled = $state(true);
	let resets = new SvelteSet<string>();
	let version = $state(0);
	let saving = $state(false);
	let error = $state<string | null>(null);
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

	function reload() {
		draft = fresh();
		enabled = sectionOn;
		resets.clear();
		error = null;
		version += 1;
	}

	// A fresh view, after a save or a reload, replaces the draft. Only `view` is watched:
	// the reset writes the draft, the toggle and the version, which must not re-run this.
	$effect.pre(() => {
		void view;
		untrack(reload);
	});

	function useDefault(key: string) {
		draft[key] = at(view.defaults, key) ?? null;
		resets.add(key);
		version += 1;
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
					return set || !isBlank(value) ? null : 'A value is needed.';
				}
				return problemOf(field, value);
			})
			.filter((p): p is string => p !== null);
	});
	const mapsValid = $derived((spec.maps ?? []).every((map) => maps[map.key]?.valid() ?? true));

	/** What saving would send. */
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
	const highlighted = $derived(
		highlight !== null && (highlight === spec.key || highlight.startsWith(`${spec.key}.`))
	);

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		saving = true;
		error = null;
		try {
			await onsave(change);
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

<form class={['card', highlighted && 'highlighted']} id={`section-${spec.key}`} onsubmit={save} novalidate>
	<div class="card-header">
		<div class="title">
			<span class="glyph"><Icon name={spec.icon} size={16} /></span>
			<div>
				<h2>{spec.title}</h2>
				<p class="hint">{spec.description}</p>
			</div>
		</div>
		<div class="row">
			<code class="small key">{spec.key}</code>
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
			<div class="error-box"><Alert tone="danger" message={error} onclose={() => (error = null)} /></div>
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
			{#key version}
				{#each spec.fields as field (field.name)}
					{@const key = fieldKey(field.name)}
					{@const source = sourceOf(view, key)}
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
						dormant={spec.optional !== undefined && !sectionOn}
						onreset={() => useDefault(key)}
					/>
				{/each}
			{/key}
			{#each spec.maps ?? [] as map (map.key)}
				{@const source = sourceOf(view, map.key)}
				{#key version}
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
			<p class="faint small">Everything here lives in the sections below.</p>
		{/if}
	</div>
	<div class="card-footer">
		{#if spec.optional && stored}
			<Button variant="ghost" onclick={resetSection} disabled={saving}>Use defaults for all of this</Button>
		{/if}
		<span class="grow"></span>
		{#if fieldProblems.length}<span class="error-text">Fix the highlighted fields.</span>{/if}
		<Button variant="ghost" onclick={reload} disabled={!dirty || saving}>Discard</Button>
		<Button type="submit" variant="primary" loading={saving} disabled={!ready}>Save</Button>
	</div>
</form>

<style>
	.title {
		display: flex;
		align-items: flex-start;
		gap: 12px;
		min-width: 0;
	}

	.glyph {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border-radius: 9px;
		background: var(--accent-soft);
		color: var(--accent-text);
		flex: none;
		margin-top: 2px;
	}

	.key {
		color: var(--text-3);
	}

	.toggle {
		padding: 4px 0 12px;
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
	}

	.highlighted {
		border-color: var(--accent);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
	}
</style>
