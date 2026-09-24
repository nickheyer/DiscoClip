<script lang="ts">
	import type { Snippet } from 'svelte';
	import { EMPTY } from '$lib/format';

	interface Props {
		label: string;
		/** Text shown when there is no custom content. An absent value reads "None". */
		value?: string | number | null;
		mono?: boolean;
		children?: Snippet;
	}

	let { label, value, mono = false, children }: Props = $props();
</script>

<dt class="text-surface-600-400">{label}</dt>
<dd class="min-w-0 break-words {mono ? 'font-mono text-xs' : ''}">
	{#if children}
		{@render children()}
	{:else if value === null || value === undefined || value === ''}
		<span class="text-surface-600-400">{EMPTY}</span>
	{:else}
		{value}
	{/if}
</dd>
