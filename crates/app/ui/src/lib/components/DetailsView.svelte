<script lang="ts">
	import JsonView from './JsonView.svelte';

	let { details }: { details: Record<string, unknown> } = $props();

	const AFTER_KEYS = ['value', 'rule', 'scope'];

	const afterKey = $derived(
		'previous' in details ? AFTER_KEYS.find((key) => key in details) : undefined
	);
	const renamed = $derived('previous_name' in details && 'name' in details);
	const rest = $derived(
		Object.entries(details).filter(([key]) => {
			if (afterKey && (key === 'previous' || key === afterKey)) return false;
			if (renamed && (key === 'previous_name' || key === 'name')) return false;
			return true;
		})
	);

	function isEmptyObject(value: unknown): boolean {
		return (
			typeof value === 'object' &&
			value !== null &&
			!Array.isArray(value) &&
			Object.keys(value).length === 0
		);
	}

	function label(key: string): string {
		return key.replace(/_/g, ' ');
	}
</script>

<div class="details">
	{#if afterKey}
		<div class="pair">
			<JsonView label="Before" value={details.previous} />
			<JsonView label="After" value={details[afterKey]} />
		</div>
	{/if}
	{#if renamed}
		<p>
			Renamed from <code>{String(details.previous_name)}</code> to <code>{String(details.name)}</code>
		</p>
	{/if}
	{#if rest.length}
		<dl class="kv">
			{#each rest as [key, value] (key)}
				<dt>{label(key)}</dt>
				<dd>
					{#if value === null || value === undefined}
						<span class="faint">—</span>
					{:else if typeof value === 'boolean'}
						{value ? 'Yes' : 'No'}
					{:else if typeof value === 'object'}
						{#if isEmptyObject(value)}
							<span class="faint">nothing</span>
						{:else}
							<JsonView {value} />
						{/if}
					{:else if typeof value === 'string' && /^[0-9a-f-]{20,}$/i.test(value)}
						<code>{value}</code>
					{:else}
						<span class="break">{String(value)}</span>
					{/if}
				</dd>
			{/each}
		</dl>
	{/if}
</div>

<style>
	.details {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}

	.pair {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
		gap: 12px;
	}
</style>
