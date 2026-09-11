<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		label: string;
		for?: string;
		hint?: string;
		error?: string | null;
		optional?: boolean;
		children: Snippet;
	}

	let { label, for: htmlFor, hint, error = null, optional = false, children }: Props = $props();
</script>

<div class="field">
	<label class="label" for={htmlFor}>
		{label}
		{#if optional}<span class="optional">optional</span>{/if}
	</label>
	{@render children()}
	{#if error}
		<p class="error-text" role="alert">{error}</p>
	{:else if hint}
		<p class="hint">{hint}</p>
	{/if}
</div>

<style>
	.field {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}

	.label {
		display: flex;
		align-items: baseline;
		gap: 8px;
		font-size: 13px;
		font-weight: 500;
		color: var(--text);
	}

	.optional {
		font-size: 11.5px;
		font-weight: 400;
		color: var(--text-3);
	}
</style>
