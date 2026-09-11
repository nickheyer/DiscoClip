<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	interface Props {
		tone?: 'info' | 'ok' | 'warn' | 'danger';
		title?: string;
		message?: string;
		children?: Snippet;
		onclose?: () => void;
	}

	let { tone = 'info', title, message, children, onclose }: Props = $props();

	const icon = $derived(
		tone === 'danger' ? 'x-circle' : tone === 'warn' ? 'warning' : tone === 'ok' ? 'check-circle' : 'info'
	);
</script>

<div class={['alert', `alert-${tone}`]} role={tone === 'danger' ? 'alert' : 'status'}>
	<span class="alert-icon"><Icon name={icon} size={16} /></span>
	<div class="alert-body">
		{#if title}<p class="alert-title">{title}</p>{/if}
		{#if message}<p>{message}</p>{/if}
		{#if children}{@render children()}{/if}
	</div>
	{#if onclose}
		<button type="button" class="alert-close" onclick={onclose} aria-label="Dismiss">
			<Icon name="x" size={14} />
		</button>
	{/if}
</div>

<style>
	.alert {
		display: flex;
		gap: 10px;
		padding: 11px 13px;
		border-radius: var(--radius-sm);
		border: 1px solid;
		font-size: 13.5px;
		line-height: 1.45;
	}

	.alert-body {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
		overflow-wrap: anywhere;
	}

	.alert-title {
		font-weight: 600;
	}

	.alert-icon {
		display: flex;
		margin-top: 2px;
	}

	.alert-close {
		border: none;
		background: transparent;
		color: inherit;
		opacity: 0.7;
		cursor: pointer;
		padding: 2px;
		border-radius: 4px;
		display: flex;
		align-self: flex-start;
	}

	.alert-close:hover {
		opacity: 1;
	}

	.alert-info {
		background: var(--info-soft);
		border-color: color-mix(in srgb, var(--info) 30%, transparent);
		color: var(--info-text);
	}

	.alert-ok {
		background: var(--ok-soft);
		border-color: color-mix(in srgb, var(--ok) 30%, transparent);
		color: var(--ok-text);
	}

	.alert-warn {
		background: var(--warn-soft);
		border-color: color-mix(in srgb, var(--warn) 30%, transparent);
		color: var(--warn-text);
	}

	.alert-danger {
		background: var(--danger-soft);
		border-color: color-mix(in srgb, var(--danger) 30%, transparent);
		color: var(--danger-text);
	}
</style>
