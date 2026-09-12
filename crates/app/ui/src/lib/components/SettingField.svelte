<script lang="ts">
	import Badge from './Badge.svelte';
	import Button from './Button.svelte';
	import Field from './Field.svelte';
	import PasswordInput from './PasswordInput.svelte';
	import TagInput from './TagInput.svelte';
	import Time from './Time.svelte';
	import type { SettingValue } from '$lib/api';
	import {
		BYTE_UNITS,
		SOURCE_LABELS,
		TIME_UNITS,
		isBlank,
		problemOf,
		unitFor,
		type ValueSource
	} from '$lib/settings/model';
	import type { FieldSpec } from '$lib/settings/schema';
	import { formatBytes, formatDuration } from '$lib/format';

	interface Props {
		id: string;
		spec: FieldSpec;
		value: SettingValue | undefined;
		/** What the server runs on now. */
		effective: SettingValue | undefined;
		fallback: SettingValue | undefined;
		source: ValueSource;
		updatedAt: string | null;
		/** For secrets: whether one is set on the server. */
		secretSet?: boolean;
		disabled?: boolean;
		/** Whether the field is within a section that is off; nothing is shown as stored. */
		dormant?: boolean;
		onreset?: () => void;
	}

	let {
		id,
		spec,
		value = $bindable(),
		effective,
		fallback,
		source,
		updatedAt,
		secretSet = false,
		disabled = false,
		dormant = false,
		onreset
	}: Props = $props();

	const problem = $derived(problemOf(spec, value));
	const dirty = $derived(JSON.stringify(value ?? null) !== JSON.stringify(effective ?? null));
	const canReset = $derived(
		!dormant && source !== 'default' && spec.kind !== 'secret' && onreset !== undefined
	);

	function describe(v: SettingValue | undefined): string {
		if (isBlank(v)) return spec.nullLabel ?? 'empty';
		if (spec.kind === 'bytes' && typeof v === 'number') return formatBytes(v);
		if (spec.kind === 'seconds' && typeof v === 'number') return formatDuration(v);
		if (spec.kind === 'days' && typeof v === 'number') return `${v} day${v === 1 ? '' : 's'}`;
		if (spec.kind === 'millis' && typeof v === 'number') return `${v} ms`;
		if (spec.kind === 'boolean') return v ? 'on' : 'off';
		if (spec.kind === 'enum') return spec.options?.find((o) => o.value === v)?.label ?? String(v);
		if (Array.isArray(v)) return v.length ? v.join(', ') : 'none';
		return String(v);
	}

	// Amounts are entered in a unit of their own and stored in the base unit.
	const units = $derived(spec.kind === 'bytes' ? BYTE_UNITS : spec.kind === 'seconds' ? TIME_UNITS : null);
	let unit = $state('');
	let amount = $state('');
	let numberText = $state('');
	let secretDraft = $state('');
	let initialized = $state(false);

	$effect.pre(() => {
		if (initialized) return;
		initialized = true;
		if (units) {
			const n = typeof value === 'number' ? value : null;
			unit = n === null ? units[0]!.label : unitFor(n, units);
			const factor = units.find((u) => u.label === unit)!.factor;
			amount = n === null ? '' : String(+(n / factor).toFixed(3));
		} else if (typeof value === 'number') {
			numberText = String(value);
		}
	});

	function commitAmount() {
		if (!units) return;
		const factor = units.find((u) => u.label === unit)!.factor;
		const text = amount.trim();
		if (text === '') {
			value = spec.nullable ? null : undefined;
			return;
		}
		const n = Number(text);
		value = Number.isFinite(n) ? Math.round(n * factor) : text;
	}

	function commitNumber() {
		const text = numberText.trim();
		if (text === '') {
			value = spec.nullable ? null : undefined;
			return;
		}
		const n = Number(text);
		value = Number.isFinite(n) ? n : text;
	}

	function commitSecret() {
		value = secretDraft === '' ? undefined : secretDraft;
	}

	function commitText(text: string) {
		const trimmed = text.trim();
		value = trimmed === '' ? (spec.nullable ? null : '') : trimmed;
	}
</script>

<div class={['setting', dirty && 'dirty']} id={`setting-${id}`}>
	<Field label={spec.label} for={id} hint={spec.hint || undefined} error={problem}>
		{#if spec.kind === 'boolean'}
			<label class="checkbox">
				<input
					{id}
					type="checkbox"
					checked={value === true}
					{disabled}
					onchange={(e) => (value = (e.currentTarget as HTMLInputElement).checked)}
				/>
				<span><span class="strong">{value ? 'On' : 'Off'}</span></span>
			</label>
		{:else if spec.kind === 'enum'}
			<select
				{id}
				class="select"
				value={typeof value === 'string' ? value : ''}
				{disabled}
				onchange={(e) => (value = (e.currentTarget as HTMLSelectElement).value)}
			>
				{#each spec.options ?? [] as option (option.value)}
					<option value={option.value}>{option.label}</option>
				{/each}
			</select>
		{:else if spec.kind === 'secret'}
			<PasswordInput
				{id}
				bind:value={secretDraft}
				placeholder={secretSet ? 'Enter a new secret to replace the one set' : 'Enter the secret'}
				mono
				{disabled}
				autocomplete="off"
				oninput={commitSecret}
			/>
		{:else if spec.kind === 'list'}
			<TagInput
				{id}
				values={Array.isArray(value) ? value.map(String) : []}
				placeholder={spec.placeholder ?? 'Type and press Enter'}
				validate={spec.validate}
				{disabled}
				onchange={(items) => (value = items)}
			/>
		{:else if units}
			<div class="unit">
				<input
					{id}
					class="input"
					type="number"
					min="0"
					step="any"
					bind:value={amount}
					oninput={commitAmount}
					placeholder={spec.nullLabel ?? ''}
					{disabled}
					aria-invalid={problem ? 'true' : undefined}
				/>
				<select class="select" bind:value={unit} onchange={commitAmount} aria-label="Unit" {disabled}>
					{#each units as u (u.label)}
						<option value={u.label}>{u.label}</option>
					{/each}
				</select>
			</div>
		{:else if spec.kind === 'integer' || spec.kind === 'number' || spec.kind === 'days' || spec.kind === 'millis'}
			<div class="unit">
				<input
					{id}
					class="input"
					type="number"
					min={spec.min ?? 0}
					step={spec.kind === 'number' ? 'any' : 1}
					bind:value={numberText}
					oninput={commitNumber}
					placeholder={spec.nullLabel ?? ''}
					{disabled}
					aria-invalid={problem ? 'true' : undefined}
				/>
				{#if spec.kind === 'days'}<span class="unit-label">days</span>{/if}
				{#if spec.kind === 'millis'}<span class="unit-label">ms</span>{/if}
			</div>
		{:else}
			<input
				{id}
				class={['input', (spec.kind === 'path' || spec.kind === 'url' || spec.kind === 'socket') && 'mono']}
				type="text"
				value={typeof value === 'string' ? value : ''}
				oninput={(e) => commitText((e.currentTarget as HTMLInputElement).value)}
				placeholder={spec.placeholder ?? spec.nullLabel ?? ''}
				spellcheck="false"
				autocomplete="off"
				{disabled}
				aria-invalid={problem ? 'true' : undefined}
			/>
		{/if}
	</Field>
	<div class="meta">
		{#if spec.kind === 'secret'}
			{#if secretSet}
				<Badge tone="ok" size="sm">Set</Badge>
			{:else}
				<Badge size="sm">Not set</Badge>
			{/if}
		{:else if !dormant}
			<Badge tone={source === 'app' ? 'accent' : source === 'provisioning' ? 'info' : 'neutral'} size="sm" title={updatedAt ? `Stored ${updatedAt}` : undefined}>
				{SOURCE_LABELS[source]}
			</Badge>
			{#if updatedAt && source !== 'default'}
				<span class="faint small"><Time value={updatedAt} /></span>
			{/if}
		{/if}
		{#if spec.kind !== 'secret'}
			<span class="faint small">Default: {describe(fallback)}</span>
		{/if}
		{#if canReset}
			<Button size="sm" variant="link" onclick={onreset} {disabled}>Use default</Button>
		{/if}
		{#if dirty}<Badge tone="warn" size="sm">Changed</Badge>{/if}
	</div>
</div>

<style>
	.setting {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 12px 0;
		border-bottom: 1px solid var(--border);
	}

	.setting:last-child {
		border-bottom: none;
	}

	.meta {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
		font-size: 12.5px;
	}

	.unit {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 6px;
		align-items: center;
	}

	.unit .select {
		width: 110px;
	}

	.unit-label {
		color: var(--text-3);
		font-size: 12.5px;
		padding: 0 6px;
	}
</style>
