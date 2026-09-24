<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		title: string;
		description?: string;
		icon?: Snippet;
		/** An action that fills the emptiness, such as a Create button. */
		children?: Snippet;
		/** Use the containing panel's border and background. */
		contained?: boolean;
	}

	let { title, description, icon, children, contained = false }: Props = $props();
</script>

<div
	class="flex min-h-56 flex-col items-center justify-center gap-3 px-6 py-10 text-center {contained
		? ''
		: 'card border border-surface-200-800 bg-surface-100-900'}"
>
	{#if icon}
		<div
			class="mb-2 flex size-12 items-center justify-center rounded-container preset-tonal-surface"
		>
			{@render icon()}
		</div>
	{/if}
	<p class="text-lg font-semibold">{title}</p>
	{#if description}
		<p class="max-w-md text-sm leading-relaxed text-surface-600-400">{description}</p>
	{/if}
	{#if children}
		<div class="mt-2 flex flex-wrap justify-center gap-2">{@render children()}</div>
	{/if}
</div>
