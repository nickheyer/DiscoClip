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

	const animBackdrop =
		'transition transition-discrete opacity-0 starting:data-[state=open]:opacity-0 data-[state=open]:opacity-100';
	const animModal =
		'transition transition-discrete opacity-0 translate-y-8 starting:data-[state=open]:opacity-0 starting:data-[state=open]:translate-y-8 data-[state=open]:opacity-100 data-[state=open]:translate-y-0';
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
		<Dialog.Backdrop
			class="fixed inset-0 z-50 bg-surface-50-950/60 backdrop-blur-sm {animBackdrop}"
		/>
		<Dialog.Positioner
			class="fixed inset-0 z-50 flex items-end justify-center sm:items-center sm:p-4"
		>
			<Dialog.Content
				class="flex max-h-dvh w-full flex-col card preset-filled-surface-100-900 shadow-xl max-sm:rounded-b-none sm:max-h-[calc(100dvh-2rem)] {WIDTH[
					size
				]} {animModal}"
			>
				<header class="flex shrink-0 items-start justify-between gap-4 p-4">
					<div class="min-w-0 space-y-1">
						<Dialog.Title class="h5">{title}</Dialog.Title>
						{#if description}
							<Dialog.Description class="text-sm text-surface-600-400">
								{description}
							</Dialog.Description>
						{/if}
					</div>
					<Dialog.CloseTrigger
						class="btn-icon shrink-0 btn-icon-sm hover:preset-tonal"
						disabled={busy}
						aria-label="Close"
					>
						<XIcon />
					</Dialog.CloseTrigger>
				</header>
				<hr class="hr" />
				<article class="min-h-0 flex-1 overflow-y-auto p-4">
					{@render children()}
				</article>
				{#if footer}
					<hr class="hr" />
					<footer
						class="flex shrink-0 flex-wrap items-center justify-end gap-2 p-4 max-sm:pb-[max(1rem,env(safe-area-inset-bottom))]"
					>
						{@render footer()}
					</footer>
				{/if}
			</Dialog.Content>
		</Dialog.Positioner>
	</Portal>
</Dialog>
