<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		tone?: 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent';
		dot?: boolean;
		pulse?: boolean;
		size?: 'sm' | 'md';
		title?: string;
		children: Snippet;
	}

	let { tone = 'neutral', dot = false, pulse = false, size = 'md', title, children }: Props =
		$props();
</script>

<span class={['badge', `badge-${tone}`, `badge-${size}`]} {title}>
	{#if dot}<span class={['dot', pulse && 'pulse']} aria-hidden="true"></span>{/if}
	{@render children()}
</span>

<style>
	.badge {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 2px 9px;
		border-radius: 999px;
		font-size: 12px;
		font-weight: 500;
		line-height: 1.6;
		white-space: nowrap;
		border: 1px solid transparent;
	}

	.badge-sm {
		padding: 0 7px;
		font-size: 11.5px;
	}

	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: currentColor;
		flex: none;
	}

	.pulse {
		box-shadow: 0 0 0 0 currentColor;
		animation: pulse 1.6s ease-out infinite;
	}

	@keyframes pulse {
		0% {
			box-shadow: 0 0 0 0 color-mix(in srgb, currentColor 55%, transparent);
		}
		100% {
			box-shadow: 0 0 0 6px transparent;
		}
	}

	.badge-neutral {
		background: var(--surface-3);
		color: var(--text-2);
	}

	.badge-ok {
		background: var(--ok-soft);
		color: var(--ok-text);
	}

	.badge-warn {
		background: var(--warn-soft);
		color: var(--warn-text);
	}

	.badge-danger {
		background: var(--danger-soft);
		color: var(--danger-text);
	}

	.badge-info {
		background: var(--info-soft);
		color: var(--info-text);
	}

	.badge-accent {
		background: var(--accent-soft);
		color: var(--accent-text);
	}
</style>
