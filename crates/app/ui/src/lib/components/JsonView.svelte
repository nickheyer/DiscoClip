<script lang="ts">
	import { prettyJson } from '$lib/format';

	let { value, label }: { value: unknown; label?: string } = $props();
	const text = $derived(prettyJson(value));
</script>

<div class="json">
	{#if label}<span class="json-label">{label}</span>{/if}
	<pre>{text}</pre>
</div>

<style>
	.json {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}

	.json-label {
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-3);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	pre {
		padding: 10px 12px;
		border-radius: var(--radius-sm);
		background: var(--surface-2);
		border: 1px solid var(--border);
		max-height: 320px;
		overflow: auto;
	}
</style>
