<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		label: string;
		/** Text shown when there is no custom content. Without one, the row is not rendered. */
		value?: string | number | null;
		mono?: boolean;
		/** A longer word for the label, shown on hover. */
		title?: string;
		children?: Snippet;
	}

	let { label, value, mono = false, title, children }: Props = $props();

	const present = $derived(
		children !== undefined || (value !== null && value !== undefined && value !== '')
	);
</script>

{#if present}
	<dt class="text-surface-600-400" {title}>{label}</dt>
	<dd class="min-w-0 break-words {mono ? 'font-mono text-xs' : ''}">
		{#if children}
			{@render children()}
		{:else}
			{value}
		{/if}
	</dd>
{/if}
