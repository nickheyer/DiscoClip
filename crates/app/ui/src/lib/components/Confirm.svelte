<script lang="ts">
	import { Dialog, Portal } from '@skeletonlabs/skeleton-svelte';
	import type { Snippet } from 'svelte';
	import { reportError } from '$lib/toast.svelte';
	import Spinner from './Spinner.svelte';

	interface Props {
		open?: boolean;
		title: string;
		message?: string;
		confirmLabel?: string;
		cancelLabel?: string;
		/** The action destroys something. */
		danger?: boolean;
		onconfirm: () => Promise<void> | void;
		children?: Snippet;
	}

	let {
		open = $bindable(false),
		title,
		message,
		confirmLabel = 'Confirm',
		cancelLabel = 'Cancel',
		danger = false,
		onconfirm,
		children
	}: Props = $props();

	let pending = $state(false);

	async function confirm() {
		pending = true;
		try {
			await onconfirm();
			open = false;
		} catch (error) {
			reportError(error);
		} finally {
			pending = false;
		}
	}
</script>

<Dialog
	{open}
	onOpenChange={(details) => {
		if (!pending) open = details.open;
	}}
	role="alertdialog"
	closeOnInteractOutside={!pending}
	closeOnEscape={!pending}
>
	<Portal>
		<Dialog.Backdrop class="fixed inset-0 z-50 bg-surface-950/60 backdrop-blur-sm" />
		<Dialog.Positioner class="fixed inset-0 z-50 flex items-center justify-center p-4">
			<Dialog.Content
				class="max-h-[calc(100dvh-2rem)] w-full max-w-lg space-y-5 overflow-y-auto card border border-surface-200-800 bg-surface-100-900 p-6 shadow-xl"
			>
				<Dialog.Title class="h4 text-xl">{title}</Dialog.Title>
				{#if message}
					<Dialog.Description class="text-surface-600-400">{message}</Dialog.Description>
				{/if}
				{@render children?.()}
				<footer class="flex flex-wrap justify-end gap-3 pt-2">
					<Dialog.CloseTrigger class="btn preset-tonal" disabled={pending}>
						{cancelLabel}
					</Dialog.CloseTrigger>
					<button
						type="button"
						class="btn {danger ? 'preset-filled-error-600-400' : 'preset-filled-primary-500'}"
						onclick={confirm}
						disabled={pending}
					>
						{#if pending}<Spinner />{/if}
						{confirmLabel}
					</button>
				</footer>
			</Dialog.Content>
		</Dialog.Positioner>
	</Portal>
</Dialog>
