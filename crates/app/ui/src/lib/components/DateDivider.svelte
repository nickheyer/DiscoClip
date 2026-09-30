<script lang="ts">
	import type { Timestamp } from '$lib/api/types';
	import { date } from '$lib/format';

	interface Props {
		/** A moment on the day the divider names. */
		at: Timestamp;
		class?: string;
	}

	let { at, class: className = '' }: Props = $props();
</script>

<div class="flex items-center gap-2 py-1 text-xs font-medium text-surface-600-400 {className}">
	<span class="h-px flex-1 bg-surface-200-800"></span>
	<span>{date(at)}</span>
	<span class="h-px flex-1 bg-surface-200-800"></span>
</div>
