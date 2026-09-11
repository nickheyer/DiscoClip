<script lang="ts">
	import Icon from './Icon.svelte';
	import { toast } from '$lib/state/toast.svelte';
</script>

<div class="toaster" aria-live="polite" aria-relevant="additions">
	{#each toast.items as item (item.id)}
		<div class={['toast', `toast-${item.tone}`]} role={item.tone === 'danger' ? 'alert' : 'status'}>
			<Icon
				name={item.tone === 'ok' ? 'check-circle' : item.tone === 'danger' ? 'x-circle' : 'info'}
				size={16}
			/>
			<span class="message">{item.message}</span>
			<button type="button" class="dismiss" onclick={() => toast.dismiss(item.id)} aria-label="Dismiss">
				<Icon name="x" size={14} />
			</button>
		</div>
	{/each}
</div>

<style>
	.toaster {
		position: fixed;
		right: 16px;
		bottom: 16px;
		z-index: 100;
		display: flex;
		flex-direction: column;
		gap: 8px;
		max-width: min(420px, calc(100vw - 32px));
		pointer-events: none;
	}

	.toast {
		pointer-events: auto;
		display: flex;
		align-items: flex-start;
		gap: 10px;
		padding: 11px 12px;
		border-radius: var(--radius-sm);
		background: var(--surface);
		border: 1px solid var(--border);
		box-shadow: var(--shadow-lg);
		font-size: 13.5px;
		animation: rise 0.18s ease-out;
	}

	.toast :global(.icon) {
		margin-top: 2px;
	}

	.message {
		flex: 1;
		overflow-wrap: anywhere;
	}

	.toast-ok {
		border-left: 3px solid var(--ok);
	}

	.toast-ok :global(.icon) {
		color: var(--ok);
	}

	.toast-danger {
		border-left: 3px solid var(--danger);
	}

	.toast-danger :global(.icon) {
		color: var(--danger);
	}

	.toast-info {
		border-left: 3px solid var(--info);
	}

	.toast-info :global(.icon) {
		color: var(--info);
	}

	.dismiss {
		display: flex;
		padding: 3px;
		border: none;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
		border-radius: 4px;
	}

	.dismiss:hover {
		color: var(--text);
		background: var(--surface-3);
	}

	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(6px);
		}
		to {
			opacity: 1;
			transform: none;
		}
	}
</style>
