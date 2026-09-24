<script lang="ts">
	import XIcon from '@lucide/svelte/icons/x';
	import { Dialog, Portal } from '@skeletonlabs/skeleton-svelte';
	import type { Snippet } from 'svelte';

	interface Props {
		open?: boolean;
		title: string;
		description?: string;
		size?: 'sm' | 'md' | 'lg' | 'xl';
		/** Keep the dialog up while work is in flight. */
		busy?: boolean;
		children: Snippet;
		footer?: Snippet;
	}

	let {
		open = $bindable(false),
		title,
		description,
		size = 'md',
		busy = false,
		children,
		footer
	}: Props = $props();

	const WIDTH = { sm: 'sm:max-w-md', md: 'sm:max-w-xl', lg: 'sm:max-w-3xl', xl: 'sm:max-w-5xl' };
</script>

<Dialog
	{open}
	onOpenChange={(details) => {
		if (!busy) open = details.open;
	}}
	closeOnInteractOutside={!busy}
	closeOnEscape={!busy}
>
	<Portal>
		<Dialog.Backdrop class="fixed inset-0 z-50 bg-surface-950/60 backdrop-blur-sm" />
		<Dialog.Positioner
			class="fixed inset-0 z-50 flex items-end justify-center sm:items-center sm:p-6"
		>
			<Dialog.Content
				class="flex w-full flex-col overflow-hidden card border border-surface-200-800 bg-surface-100-900 shadow-xl max-sm:h-dvh max-sm:max-h-dvh max-sm:rounded-none sm:max-h-[calc(100dvh-3rem)] {WIDTH[
					size
				]}"
			>
				<header
					class="flex shrink-0 items-start justify-between gap-4 border-b border-surface-200-800 p-5 sm:p-6"
				>
					<div class="min-w-0 space-y-1">
						<Dialog.Title class="h4 text-xl">{title}</Dialog.Title>
						{#if description}
							<Dialog.Description class="text-sm text-surface-600-400">
								{description}
							</Dialog.Description>
						{/if}
					</div>
					<Dialog.CloseTrigger
						class="btn-icon shrink-0 hover:preset-tonal"
						disabled={busy}
						aria-label="Close"
					>
						<XIcon class="size-4" />
					</Dialog.CloseTrigger>
				</header>
				<div class="min-h-0 flex-1 overflow-y-auto p-5 sm:p-6">
					{@render children()}
				</div>
				{#if footer}
					<footer
						class="flex shrink-0 flex-wrap items-center justify-end gap-3 border-t border-surface-200-800 px-5 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] sm:px-6"
					>
						{@render footer()}
					</footer>
				{/if}
			</Dialog.Content>
		</Dialog.Positioner>
	</Portal>
</Dialog>
