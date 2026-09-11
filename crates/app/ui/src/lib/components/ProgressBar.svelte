<script lang="ts">
	import type { Progress } from '$lib/api';
	import { formatBytes } from '$lib/format';

	interface Props {
		progress: Progress;
		/** What the counts are of: `bytes`, `segments` or `time` in microseconds. */
		unit?: 'bytes' | 'segments' | 'time' | 'percent';
		label?: string;
		size?: 'sm' | 'md';
	}

	let { progress, unit = 'bytes', label, size = 'md' }: Props = $props();

	const fraction = $derived(
		progress.total && progress.total > 0 ? Math.min(1, progress.done / progress.total) : null
	);

	function show(value: number): string {
		switch (unit) {
			case 'bytes':
				return formatBytes(value);
			case 'time':
				return `${(value / 1_000_000).toFixed(0)}s`;
			case 'percent':
				return `${value}%`;
			default:
				return String(value);
		}
	}

	const text = $derived.by(() => {
		if (fraction !== null) return `${Math.round(fraction * 100)}%`;
		return `${show(progress.done)}${unit === 'segments' ? ' segments' : ''}`;
	});
</script>

<div class={['bar', `bar-${size}`]} role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={fraction === null ? undefined : Math.round(fraction * 100)} aria-label={label}>
	<div class="track">
		<div class={['fill', fraction === null && 'indeterminate']} style={fraction === null ? undefined : `width:${fraction * 100}%`}></div>
	</div>
	<span class="text mono">{text}</span>
</div>

<style>
	.bar {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
	}

	.track {
		flex: 1;
		height: 6px;
		border-radius: 999px;
		background: var(--surface-3);
		overflow: hidden;
		min-width: 60px;
	}

	.bar-sm .track {
		height: 4px;
	}

	.fill {
		height: 100%;
		border-radius: 999px;
		background: var(--accent);
		transition: width 0.25s ease-out;
	}

	.indeterminate {
		width: 35%;
		animation: slide 1.2s ease-in-out infinite;
	}

	@keyframes slide {
		0% {
			transform: translateX(-100%);
		}
		100% {
			transform: translateX(300%);
		}
	}

	.text {
		font-size: 11.5px;
		color: var(--text-3);
		white-space: nowrap;
		min-width: 3.5ch;
		text-align: right;
	}
</style>
