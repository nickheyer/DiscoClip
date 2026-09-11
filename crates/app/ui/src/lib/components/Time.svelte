<script lang="ts">
	import { formatDateTime, formatRelative } from '$lib/format';
	import { clock } from '$lib/state/clock.svelte';

	interface Props {
		value: string | null | undefined;
		/** What is shown; the other form goes in the tooltip. */
		mode?: 'relative' | 'absolute';
		empty?: string;
	}

	let { value, mode = 'relative', empty = '—' }: Props = $props();

	const absolute = $derived(value ? formatDateTime(value, true) : '');
	const relative = $derived(value ? formatRelative(value, clock.now) : '');
</script>

{#if value}
	<time datetime={value} title={mode === 'relative' ? absolute : relative} class="nowrap">
		{mode === 'relative' ? relative : formatDateTime(value)}
	</time>
{:else}
	<span class="faint">{empty}</span>
{/if}
