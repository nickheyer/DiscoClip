<script lang="ts">
	import { EMPTY, bytes, number } from '$lib/format';

	interface Props {
		value: number | null | undefined;
	}

	let { value }: Props = $props();
</script>

{#if value === null || value === undefined}
	<span class="text-surface-600-400">{EMPTY}</span>
{:else}
	<span title="{number(value)} bytes">{bytes(value)}</span>
{/if}
