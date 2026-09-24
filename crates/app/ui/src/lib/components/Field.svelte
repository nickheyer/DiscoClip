<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		label: string;
		/** The id of the control, so the label reaches it. */
		for?: string;
		help?: string;
		error?: string | null;
		required?: boolean;
		class?: string;
		children: Snippet;
	}

	let {
		label,
		for: htmlFor,
		help,
		error = null,
		required = false,
		class: className = '',
		children
	}: Props = $props();
</script>

<div class="label min-w-0 gap-2 {className}">
	<label class="label-text text-sm font-medium" for={htmlFor}>
		{label}
		{#if required}<span class="text-error-700-300" aria-hidden="true">*</span>{/if}
	</label>
	{@render children()}
	{#if error}
		<p class="text-sm text-error-700-300" role="alert">{error}</p>
	{:else if help}
		<p class="text-sm text-surface-600-400">{help}</p>
	{/if}
</div>
