<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	interface Props {
		open: boolean;
		title: string;
		description?: string;
		size?: 'sm' | 'md' | 'lg';
		/** Keeps the dialog up while work is in flight. */
		busy?: boolean;
		onclose?: () => void;
		children: Snippet;
		footer?: Snippet;
	}

	let {
		open = $bindable(false),
		title,
		description,
		size = 'md',
		busy = false,
		onclose,
		children,
		footer
	}: Props = $props();

	let element: HTMLDialogElement | undefined = $state();

	$effect(() => {
		if (!element) return;
		if (open && !element.open) {
			element.showModal();
			// The first field, not the close button, is where typing should start.
			element
				.querySelector<HTMLElement>('.body input:not([type=hidden]), .body select, .body textarea')
				?.focus();
		} else if (!open && element.open) {
			element.close();
		}
	});

	function close() {
		if (busy) return;
		open = false;
		onclose?.();
	}

	function onCancel(event: Event) {
		event.preventDefault();
		close();
	}

	function onBackdrop(event: MouseEvent) {
		if (event.target === element) close();
	}
</script>

<dialog
	bind:this={element}
	class={['dialog', `dialog-${size}`]}
	oncancel={onCancel}
	onclick={onBackdrop}
	aria-labelledby="dialog-title"
>
	<div class="panel">
		<header class="head">
			<div>
				<h2 id="dialog-title">{title}</h2>
				{#if description}<p class="muted small">{description}</p>{/if}
			</div>
			<button type="button" class="close" onclick={close} aria-label="Close" disabled={busy}>
				<Icon name="x" size={16} />
			</button>
		</header>
		<div class="body">{@render children()}</div>
		{#if footer}
			<footer class="foot">{@render footer()}</footer>
		{/if}
	</div>
</dialog>

<style>
	.dialog {
		padding: 0;
		border: none;
		background: transparent;
		max-width: min(calc(100vw - 32px), var(--w));
		width: 100%;
		--w: 520px;
	}

	.dialog-sm {
		--w: 420px;
	}

	.dialog-lg {
		--w: 720px;
	}

	.dialog::backdrop {
		background: rgb(10 12 18 / 0.55);
		backdrop-filter: blur(2px);
	}

	.panel {
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: 14px;
		box-shadow: var(--shadow-lg);
		color: var(--text);
		display: flex;
		flex-direction: column;
		max-height: calc(100vh - 48px);
	}

	.head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 12px;
		padding: 18px 20px 12px;
	}

	.head h2 {
		font-size: 16px;
	}

	.close {
		display: flex;
		padding: 6px;
		border: none;
		border-radius: 6px;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
		margin: -4px -6px 0 0;
	}

	.close:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--text);
	}

	.body {
		padding: 4px 20px 20px;
		overflow: auto;
	}

	.foot {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		padding: 12px 20px;
		border-top: 1px solid var(--border);
		background: var(--surface-2);
		border-radius: 0 0 14px 14px;
	}

	@media (max-width: 600px) {
		.foot {
			flex-direction: column-reverse;
		}

		.foot :global(.btn) {
			width: 100%;
		}
	}
</style>
