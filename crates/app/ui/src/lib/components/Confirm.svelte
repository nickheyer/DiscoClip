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

	const animModal =
		'transition transition-discrete opacity-0 scale-95 starting:data-[state=open]:opacity-0 starting:data-[state=open]:scale-95 data-[state=open]:opacity-100 data-[state=open]:scale-100';
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
		<Dialog.Backdrop
			class="fixed inset-0 z-50 backdrop-blur-sm {danger
				? 'bg-error-50-950/50'
				: 'bg-surface-50-950/60'}"
		/>
		<Dialog.Positioner class="fixed inset-0 z-50 flex items-center justify-center p-4">
			<Dialog.Content
				class="w-full max-w-md space-y-4 card preset-filled-surface-100-900 p-4 shadow-xl {animModal}"
			>
				<Dialog.Title class="h5">{title}</Dialog.Title>
				{#if message}
					<Dialog.Description class="text-sm text-surface-600-400">{message}</Dialog.Description>
				{/if}
				{@render children?.()}
				<footer class="flex flex-wrap justify-end gap-2">
					<Dialog.CloseTrigger class="btn preset-tonal" disabled={pending}>
						{cancelLabel}
					</Dialog.CloseTrigger>
					<button
						type="button"
						class="btn {danger ? 'preset-filled-error-500' : 'preset-filled-primary-500'}"
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
