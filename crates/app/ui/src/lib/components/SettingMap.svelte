<script lang="ts">
	import Badge from './Badge.svelte';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import Time from './Time.svelte';
	import type { SettingValue } from '$lib/api';
	import { SOURCE_LABELS, problemOf, type ValueSource } from '$lib/settings/model';
	import { HOST_RE, type MapSpec } from '$lib/settings/schema';

	interface Row {
		key: string;
		values: Record<string, SettingValue>;
	}

	interface Props {
		spec: MapSpec;
		value: SettingValue | undefined;
		effective: SettingValue | undefined;
		source: ValueSource;
		updatedAt: string | null;
		disabled?: boolean;
		onreset?: () => void;
	}

	let {
		spec,
		value = $bindable(),
		effective,
		source,
		updatedAt,
		disabled = false,
		onreset
	}: Props = $props();

	function rowsOf(map: SettingValue | undefined): Row[] {
		if (!map || typeof map !== 'object' || Array.isArray(map)) return [];
		return Object.entries(map).map(([key, v]) => ({
			key,
			values: spec.scalar
				? { [spec.fields[0]!.name]: v }
				: v && typeof v === 'object' && !Array.isArray(v)
					? { ...(v as Record<string, SettingValue>) }
					: {}
		}));
	}

	let rows = $state<Row[]>([]);
	let initialized = $state(false);
	$effect.pre(() => {
		if (initialized) return;
		initialized = true;
		rows = rowsOf(value);
	});

	function commit() {
		const map: Record<string, SettingValue> = {};
		for (const row of rows) {
			const key = row.key.trim();
			if (!key) continue;
			map[key] = spec.scalar ? (row.values[spec.fields[0]!.name] ?? null) : { ...row.values };
		}
		value = map;
	}

	function add() {
		const values: Record<string, SettingValue> = {};
		for (const field of spec.fields) {
			values[field.name] = field.kind === 'integer' || field.kind === 'number' ? 0 : '';
		}
		rows = [...rows, { key: '', values }];
		commit();
	}

	function remove(index: number) {
		rows = rows.filter((_, i) => i !== index);
		commit();
	}

	function keyProblem(key: string): string | null {
		const text = key.trim();
		if (!text) return 'A name is needed.';
		if (spec.keyLabel === 'Host' && !HOST_RE.test(text)) return `${text} is not a host name`;
		if (rows.filter((r) => r.key.trim() === text).length > 1) return `${text} is listed twice`;
		return null;
	}

	const dirty = $derived(JSON.stringify(value ?? {}) !== JSON.stringify(effective ?? {}));
	const problems = $derived(
		rows.some(
			(row) =>
				keyProblem(row.key) !== null ||
				spec.fields.some((field) => problemOf(field, row.values[field.name]) !== null)
		)
	);

	export function valid(): boolean {
		return !problems;
	}
</script>

<div class={['map', dirty && 'dirty']} id={`setting-${spec.key}`}>
	<div class="head">
		<div>
			<span class="label">{spec.label}</span>
			<p class="hint">{spec.hint}</p>
		</div>
		<Button size="sm" icon="plus" onclick={add} {disabled}>Add</Button>
	</div>
	{#if rows.length === 0}
		<p class="faint small">Nothing listed.</p>
	{:else}
		<div class="table-wrap inner">
			<table class="table">
				<thead>
					<tr>
						<th>{spec.keyLabel}</th>
						{#each spec.fields as field (field.name)}
							<th>{field.label}</th>
						{/each}
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each rows as row, index (index)}
						{@const problem = keyProblem(row.key)}
						<tr>
							<td>
								<input
									class="input mono"
									value={row.key}
									placeholder={spec.keyPlaceholder}
									spellcheck="false"
									autocomplete="off"
									{disabled}
									aria-label={spec.keyLabel}
									aria-invalid={problem ? 'true' : undefined}
									title={problem ?? undefined}
									oninput={(e) => {
										row.key = (e.currentTarget as HTMLInputElement).value;
										commit();
									}}
								/>
							</td>
							{#each spec.fields as field (field.name)}
								{@const fieldProblem = problemOf(field, row.values[field.name])}
								<td>
									{#if field.kind === 'integer' || field.kind === 'number'}
										<input
											class="input"
											type="number"
											min={field.min ?? 0}
											step={field.kind === 'number' ? 'any' : 1}
											value={row.values[field.name] ?? ''}
											{disabled}
											aria-label={field.label}
											aria-invalid={fieldProblem ? 'true' : undefined}
											title={fieldProblem ?? undefined}
											oninput={(e) => {
												const text = (e.currentTarget as HTMLInputElement).value.trim();
												const n = Number(text);
												row.values[field.name] = text === '' ? null : Number.isFinite(n) ? n : text;
												commit();
											}}
										/>
									{:else}
										<input
											class="input mono"
											value={typeof row.values[field.name] === 'string' ? row.values[field.name] : ''}
											placeholder={field.placeholder}
											spellcheck="false"
											autocomplete="off"
											{disabled}
											aria-label={field.label}
											aria-invalid={fieldProblem ? 'true' : undefined}
											title={fieldProblem ?? undefined}
											oninput={(e) => {
												row.values[field.name] = (e.currentTarget as HTMLInputElement).value.trim();
												commit();
											}}
										/>
									{/if}
								</td>
							{/each}
							<td class="actions">
								<button type="button" class="remove" onclick={() => remove(index)} aria-label="Remove" {disabled}>
									<Icon name="x" size={14} />
								</button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<div class="meta">
		<Badge tone={source === 'app' ? 'accent' : source === 'provisioning' ? 'info' : 'neutral'} size="sm">{SOURCE_LABELS[source]}</Badge>
		{#if updatedAt && source !== 'default'}<span class="faint small"><Time value={updatedAt} /></span>{/if}
		<span class="faint small">Default: nothing listed</span>
		{#if source !== 'default' && onreset}
			<Button size="sm" variant="link" onclick={onreset} {disabled}>Use default</Button>
		{/if}
		{#if dirty}<Badge tone="warn" size="sm">Changed</Badge>{/if}
		{#if problems}<span class="error-text">Fix the highlighted entries.</span>{/if}
	</div>
</div>

<style>
	.map {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 12px 0;
		border-bottom: 1px solid var(--border);
	}

	.map:last-child {
		border-bottom: none;
	}

	.head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 12px;
	}

	.label {
		font-size: 13px;
		font-weight: 500;
	}

	.inner {
		box-shadow: none;
	}

	.inner .table td {
		padding: 6px 8px;
	}

	.remove {
		display: flex;
		padding: 4px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
	}

	.remove:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--danger-text);
	}

	.meta {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
		font-size: 12.5px;
	}
</style>
