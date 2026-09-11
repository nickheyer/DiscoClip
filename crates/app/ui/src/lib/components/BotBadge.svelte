<script lang="ts" module>
	import type { BotStateName } from '$lib/api';

	export const BOT_STATE_LABELS: Record<BotStateName, string> = {
		disabled: 'No token',
		stopped: 'Stopped',
		starting: 'Starting',
		connected: 'Connected',
		retrying: 'Retrying',
		failed: 'Failed'
	};

	export function botTone(
		state: BotStateName
	): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent' {
		switch (state) {
			case 'connected':
				return 'ok';
			case 'starting':
				return 'info';
			case 'retrying':
				return 'warn';
			case 'failed':
				return 'danger';
			default:
				return 'neutral';
		}
	}
</script>

<script lang="ts">
	import Badge from './Badge.svelte';

	let { state, size = 'md' }: { state: BotStateName; size?: 'sm' | 'md' } = $props();
	const live = $derived(state === 'starting' || state === 'retrying');
</script>

<Badge tone={botTone(state)} dot pulse={live} {size}>{BOT_STATE_LABELS[state]}</Badge>
