<script lang="ts">
	import { bytes, number } from '$lib/format';

	interface Props {
		value: number | null | undefined;
	}

	let { value }: Props = $props();
</script>

{#if value !== null && value !== undefined}
	<span title="{number(value)} bytes">{bytes(value)}</span>
{/if}
