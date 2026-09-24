<script lang="ts">
	import { clock, EMPTY } from '$lib/format';

	interface Props {
		/** Seconds. */
		value: number | null | undefined;
	}

	let { value }: Props = $props();
</script>

{#if value === null || value === undefined}
	<span class="text-surface-600-400">{EMPTY}</span>
{:else}
	<span class="tabular-nums">{clock(value)}</span>
{/if}
