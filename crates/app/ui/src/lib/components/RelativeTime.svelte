<script lang="ts">
	import { NEVER, absolute, relative } from '$lib/format';
	import { now } from '$lib/now.svelte';
	import type { Timestamp } from '$lib/api/types';

	interface Props {
		at: Timestamp | null | undefined;
		class?: string;
	}

	let { at, class: className = '' }: Props = $props();

	now.start();
</script>

{#if at}
	<time datetime={at} title={absolute(at)} class={className}>{relative(at, now.value)}</time>
{:else}
	<span class="text-surface-600-400 {className}">{NEVER}</span>
{/if}
