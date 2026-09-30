<script lang="ts">
	import type { Timestamp as Iso } from '$lib/api/types';
	import { absolute, clockTime, relative } from '$lib/format';
	import { now } from '$lib/now.svelte';

	interface Props {
		at: Iso | null | undefined;
		/** How the time reads: relative to now, with the whole date and time as its title, or
		 * as the clock time of its day, for a line in a log. */
		mode?: 'relative' | 'clock';
		class?: string;
	}

	let { at, mode = 'relative', class: className = '' }: Props = $props();

	now.start();
</script>

{#if at}
	<time
		datetime={at}
		title={absolute(at)}
		class="{mode === 'clock' ? 'tabular-nums' : ''} {className}"
		>{mode === 'clock' ? clockTime(at) : relative(at, now.value)}</time
	>
{/if}
