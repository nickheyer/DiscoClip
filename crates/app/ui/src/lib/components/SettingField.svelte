<script lang="ts">
	import PencilIcon from '@lucide/svelte/icons/pencil';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import type { Json } from '$lib/api/types';
	import { EMPTY } from '$lib/format';
	import {
		display,
		rawNumber,
		setAt,
		summary,
		valueAt,
		type Leaf,
		type SettingField
	} from '$lib/settings';
	import Field from './Field.svelte';
	import RelativeTime from './RelativeTime.svelte';
	import Spinner from './Spinner.svelte';

	interface Props {
		field: SettingField;
		onsave: (key: string, value: Json) => Promise<void>;
		onreset: (key: string) => Promise<void>;
	}

	let { field, onsave, onreset }: Props = $props();

	let editing = $state(false);
	let pending = $state<'save' | 'reset' | null>(null);
	let text = $state('');
	let checked = $state(false);
	/** The text of each value of a section being edited, by its name. */
	let parts = $state<Record<string, string>>({});
	/** The switches of a section being edited, by name. */
	let flags = $state<Record<string, boolean>>({});
	let error = $state<string | null>(null);

	/** How a value sits in a text box. */
	function textOf(value: Json | undefined, leaf: Leaf): string {
		if (value === null || value === undefined) return '';
		switch (leaf.kind) {
			case 'list':
				return Array.isArray(value) ? value.map(String).join('\n') : '';
			case 'json':
				return JSON.stringify(value, null, 2);
			default:
				return String(value);
		}
	}

	function begin() {
		error = null;
		switch (field.kind) {
			case 'section': {
				const next: Record<string, string> = {};
				const on: Record<string, boolean> = {};
				for (const leaf of field.leaves ?? []) {
					const held = valueAt(field.value, leaf.name);
					if (leaf.kind === 'boolean') on[leaf.name] = held === true;
					else next[leaf.name] = leaf.secret ? '' : textOf(held, leaf);
				}
				parts = next;
				flags = on;
				break;
			}
			case 'boolean':
				checked = field.value === true;
				break;
			case 'list':
				text = Array.isArray(field.value) ? field.value.map(String).join('\n') : '';
				break;
			case 'json':
				text =
					field.value === null || field.value === undefined
						? ''
						: JSON.stringify(field.value, null, 2);
				break;
			default:
				text = field.secret
					? ''
					: field.value === null || field.value === undefined
						? ''
						: String(field.value);
		}
		editing = true;
	}

	/** The section as an object, from what was typed. Blank values are left out, so the
	 * server's defaults fill them; a stored secret left blank is sent as null, which keeps it. */
	function parseSection(): Json {
		const out: Record<string, Json> = {};
		let filled = 0;
		for (const leaf of field.leaves ?? []) {
			if (leaf.kind === 'boolean') {
				setAt(out, leaf.name, flags[leaf.name] === true);
				filled += 1;
				continue;
			}
			const typed = (parts[leaf.name] ?? '').trim();
			if (!typed) {
				if (leaf.secret && leaf.set) setAt(out, leaf.name, null);
				continue;
			}
			filled += 1;
			switch (leaf.kind) {
				case 'number': {
					const n = Number(typed);
					if (!Number.isFinite(n)) throw new Error(`${leaf.name}: that is not a number.`);
					setAt(out, leaf.name, n);
					break;
				}
				case 'list':
					setAt(
						out,
						leaf.name,
						typed
							.split('\n')
							.map((line) => line.trim())
							.filter(Boolean)
					);
					break;
				case 'json':
					try {
						setAt(out, leaf.name, JSON.parse(typed) as Json);
					} catch {
						throw new Error(`${leaf.name}: that is not valid JSON.`);
					}
					break;
				default:
					setAt(out, leaf.name, typed);
			}
		}
		if (filled === 0) throw new Error('Enter at least one value, or leave the section off.');
		return out;
	}

	function parse(): Json {
		switch (field.kind) {
			case 'section':
				return parseSection();
			case 'boolean':
				return checked;
			case 'number': {
				if (!text.trim()) throw new Error('Enter a number.');
				const n = Number(text);
				if (!Number.isFinite(n)) throw new Error('That is not a number.');
				return n;
			}
			case 'string':
				return text === '' && field.default === null ? null : text;
			case 'list':
				return text
					.split('\n')
					.map((line) => line.trim())
					.filter(Boolean);
			case 'json': {
				if (!text.trim()) return field.default === null ? null : {};
				try {
					return JSON.parse(text) as Json;
				} catch {
					throw new Error('That is not valid JSON.');
				}
			}
		}
	}

	async function save() {
		error = null;
		let value: Json;
		try {
			value = parse();
		} catch (err) {
			error = err instanceof Error ? err.message : String(err);
			return;
		}
		pending = 'save';
		try {
			await onsave(field.key, value);
			editing = false;
		} finally {
			pending = null;
		}
	}

	async function reset() {
		pending = 'reset';
		try {
			await onreset(field.key);
			editing = false;
		} finally {
			pending = null;
		}
	}

	const inputId = $derived(`setting-${field.key.replace(/[^a-z0-9]+/gi, '-')}`);
	const shown = $derived(
		field.kind === 'section' ? summary(field) : display(field.value, field.kind, field.unit)
	);
	const raw = $derived(rawNumber(field.value, field.unit));
	const fallback = $derived(display(field.default, field.kind, field.unit));
	/** The app wrote this value, so removing it puts the default back. */
	const resettable = $derived(field.stored?.source === 'app');
</script>

<div
	class="grid gap-3 py-3 xl:grid-cols-[minmax(16rem,24rem)_minmax(0,1fr)_auto] xl:items-start xl:gap-6"
>
	<div class="flex min-w-0 flex-wrap items-center gap-2">
		{#if field.kind === 'section'}
			<span id={inputId} class="font-mono text-sm break-all">{field.key}</span>
		{:else}
			<label for={inputId} class="font-mono text-sm break-all">{field.key}</label>
		{/if}
		{#if field.stored}
			<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
				{field.stored.source === 'provisioning' ? 'From config' : 'Saved in app'}
			</span>
		{/if}
		{#if field.secret}
			<span class="badge preset-tonal-warning" style="--badge-size: var(--text-xs)">Secret</span>
		{/if}
		{#if field.stored}
			<RelativeTime at={field.stored.updated_at} class="text-xs text-surface-600-400" />
		{/if}
	</div>

	<div class="min-w-0 text-sm">
		{#if editing}
			<div class="space-y-2">
				{#if field.kind === 'section'}
					<div class="grid gap-3 sm:grid-cols-2" role="group" aria-labelledby={inputId}>
						{#each field.leaves ?? [] as leaf (leaf.key)}
							{@const leafId = `${inputId}-${leaf.name.replace(/[^a-z0-9]+/gi, '-')}`}
							{#if leaf.kind === 'boolean'}
								<Switch
									checked={flags[leaf.name] === true}
									onCheckedChange={(details) =>
										(flags = { ...flags, [leaf.name]: details.checked })}
									class="self-end"
								>
									<Switch.Control><Switch.Thumb /></Switch.Control>
									<Switch.Label>{leaf.name}</Switch.Label>
									<Switch.HiddenInput id={leafId} />
								</Switch>
							{:else}
								<Field
									label={leaf.name}
									for={leafId}
									help={leaf.secret && leaf.set
										? 'Leave blank to keep the stored secret.'
										: undefined}
									class={leaf.kind === 'list' || leaf.kind === 'json' ? 'sm:col-span-2' : ''}
								>
									{#if leaf.choices}
										<select id={leafId} class="select" bind:value={parts[leaf.name]}>
											<option value="">Default</option>
											{#each leaf.choices as choice (choice)}<option value={choice}>{choice}</option
												>{/each}
										</select>
									{:else if leaf.kind === 'number'}
										<div class="field-group grid-cols-[1fr_auto]">
											<input
												id={leafId}
												class="input"
												type="number"
												step="any"
												placeholder={leaf.placeholder}
												bind:value={parts[leaf.name]}
											/>
											{#if leaf.unit}<div class="label label-text preset-tonal">
													{leaf.unit.label}
												</div>{/if}
										</div>
									{:else if leaf.kind === 'list'}
										<textarea
											id={leafId}
											class="textarea font-mono text-xs"
											rows="3"
											placeholder="One entry per line"
											bind:value={parts[leaf.name]}></textarea>
									{:else if leaf.kind === 'json'}
										<textarea
											id={leafId}
											class="textarea font-mono text-xs"
											rows="4"
											spellcheck="false"
											bind:value={parts[leaf.name]}></textarea>
									{:else}
										<input
											id={leafId}
											class="input {leaf.secret ? 'font-mono' : ''}"
											type={leaf.secret ? 'password' : 'text'}
											autocomplete="off"
											placeholder={leaf.secret ? (leaf.set ? 'Unchanged' : '') : leaf.placeholder}
											bind:value={parts[leaf.name]}
										/>
									{/if}
								</Field>
							{/if}
						{/each}
					</div>
				{:else if field.kind === 'boolean'}
					<Switch {checked} onCheckedChange={(details) => (checked = details.checked)}>
						<Switch.Control><Switch.Thumb /></Switch.Control>
						<Switch.Label>{checked ? 'On' : 'Off'}</Switch.Label>
						<Switch.HiddenInput id={inputId} />
					</Switch>
				{:else if field.kind === 'number'}
					<div class="field-group grid-cols-[1fr_auto]">
						<input id={inputId} class="input" type="number" step="any" bind:value={text} />
						{#if field.unit}
							<div class="label label-text preset-tonal">{field.unit.label}</div>
						{/if}
					</div>
				{:else if field.kind === 'string' && field.choices}
					<select id={inputId} class="select" bind:value={text}>
						{#each field.choices as choice (choice)}<option value={choice}>{choice}</option>{/each}
					</select>
				{:else if field.kind === 'string'}
					<input
						id={inputId}
						class="input {field.secret ? 'font-mono' : ''}"
						type={field.secret ? 'password' : 'text'}
						bind:value={text}
						autocomplete="off"
						placeholder={field.secret ? 'New value' : ''}
					/>
				{:else if field.kind === 'list'}
					<textarea
						id={inputId}
						class="textarea font-mono text-xs"
						rows="4"
						bind:value={text}
						placeholder="One entry per line"></textarea>
				{:else}
					<textarea
						id={inputId}
						class="textarea font-mono text-xs"
						rows="6"
						bind:value={text}
						spellcheck="false"></textarea>
				{/if}
				{#if error}<p class="text-xs text-error-600-400" role="alert">{error}</p>{/if}
				<p class="text-xs text-surface-600-400">Default: {fallback}</p>
			</div>
		{:else}
			<p class="break-all {field.kind === 'json' ? 'font-mono text-xs' : ''}">
				{#if field.secret}
					{#if field.value === null || field.value === undefined || field.value === ''}
						<span class="text-surface-600-400">{EMPTY}</span>
					{:else}
						••••••••
					{/if}
				{:else if shown === EMPTY}
					<span class="text-surface-600-400">{EMPTY}</span>
				{:else}
					{shown}{#if raw}<span class="ml-2 font-mono text-xs text-surface-600-400">{raw}</span
						>{/if}
				{/if}
			</p>
			{#if field.stored && !field.secret && fallback !== shown}
				<p class="text-xs text-surface-600-400">Default: {fallback}</p>
			{/if}
		{/if}
	</div>

	<div class="flex flex-wrap justify-end gap-2">
		{#if editing}
			<button
				type="button"
				class="btn preset-tonal btn-sm"
				onclick={() => (editing = false)}
				disabled={pending !== null}>Cancel</button
			>
			<button
				type="button"
				class="btn preset-filled-primary-500 btn-sm"
				onclick={save}
				disabled={pending !== null}
			>
				{#if pending === 'save'}<Spinner />{/if}
				Save
			</button>
		{:else}
			{#if resettable}
				<button
					type="button"
					class="btn btn-sm hover:preset-tonal"
					onclick={reset}
					disabled={pending !== null}
					title="Reset to default"
					aria-label="Reset {field.key} to default"
				>
					{#if pending === 'reset'}<Spinner />{:else}<RotateCcwIcon class="size-3.5" />{/if}
					Reset
				</button>
			{/if}
			<button
				type="button"
				class="btn preset-tonal btn-sm"
				onclick={begin}
				disabled={pending !== null}
			>
				<PencilIcon class="size-3.5" />
				{field.secret ? 'Replace' : 'Edit'}
			</button>
		{/if}
	</div>
</div>
