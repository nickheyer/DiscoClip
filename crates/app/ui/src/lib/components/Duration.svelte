<script lang="ts">
	import type { Timestamp } from '$lib/api/types';
	import { durationText } from '$lib/format';

	interface Props {
		/** Seconds. */
		value?: number | null;
		/** A moment the duration counts from: it ticks every second until `until` is set. */
		since?: Timestamp | null;
		/** When a duration counted from `since` stopped. */
		until?: Timestamp | null;
		class?: string;
	}

	let { value = null, since = null, until = null, class: className = '' }: Props = $props();

	let tick = $state(Date.now());

	$effect(() => {
		if (!since || until) return;
		tick = Date.now();
		const timer = setInterval(() => {
			tick = Date.now();
		}, 1000);
		return () => clearInterval(timer);
	});

	const seconds = $derived.by((): number | null => {
		if (since) {
			const start = Date.parse(since);
			if (Number.isNaN(start)) return null;
			const end = until ? Date.parse(until) : tick;
			if (Number.isNaN(end)) return null;
			return Math.max(0, (end - start) / 1000);
		}
		return value ?? null;
	});
</script>

{#if seconds !== null}
	<span class="tabular-nums {className}">{durationText(seconds)}</span>
{/if}
