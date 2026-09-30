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

<!-- Skeleton's label and label-text, with the help or error line beneath the control. -->
<div class="label min-w-0 {className}">
	<label class="label-text" for={htmlFor}>
		{label}
		{#if required}<span class="text-error-500" aria-hidden="true">*</span>{/if}
	</label>
	{@render children()}
	{#if error}
		<p class="text-xs text-error-600-400" role="alert">{error}</p>
	{:else if help}
		<p class="text-xs text-surface-600-400">{help}</p>
	{/if}
</div>
