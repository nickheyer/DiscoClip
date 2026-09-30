<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		title: string;
		description?: string;
		icon?: Snippet;
		/** An action that fills the emptiness, such as a Create button. */
		children?: Snippet;
		/** Use the containing card's surface rather than a card of its own. */
		contained?: boolean;
	}

	let { title, description, icon, children, contained = false }: Props = $props();
</script>

<div
	class="flex min-h-40 flex-col items-center justify-center gap-2 p-8 text-center {contained
		? ''
		: 'card preset-tonal'}"
>
	{#if icon}
		<div class="mb-1 flex size-10 items-center justify-center rounded-full preset-tonal">
			{@render icon()}
		</div>
	{/if}
	<p class="font-semibold">{title}</p>
	{#if description}
		<p class="max-w-md text-sm text-surface-600-400">{description}</p>
	{/if}
	{#if children}
		<div class="mt-2 flex flex-wrap justify-center gap-2">{@render children()}</div>
	{/if}
</div>
