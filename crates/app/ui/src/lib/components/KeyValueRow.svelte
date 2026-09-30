<script lang="ts">
	import type { Snippet } from 'svelte';
	import { EMPTY } from '$lib/format';

	interface Props {
		label: string;
		/** Text shown when there is no custom content. An absent value reads "None". */
		value?: string | number | null;
		/** What an absent value reads instead of "None". */
		empty?: string;
		mono?: boolean;
		/** A longer word for the label, shown on hover. */
		title?: string;
		children?: Snippet;
	}

	let { label, value, empty = EMPTY, mono = false, title, children }: Props = $props();
</script>

<dt class="text-surface-600-400" {title}>{label}</dt>
<dd class="min-w-0 break-words {mono ? 'font-mono text-xs' : ''}">
	{#if children}
		{@render children()}
	{:else if value === null || value === undefined || value === ''}
		<span class="text-surface-600-400">{empty}</span>
	{:else}
		{value}
	{/if}
</dd>
