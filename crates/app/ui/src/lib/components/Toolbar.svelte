<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		/** What the page holds. Shown at the start of the row from `lg` up. */
		description?: string;
		children?: Snippet;
		class?: string;
	}

	let { description, children, class: className = '' }: Props = $props();
</script>

<!-- A page's actions, right-aligned, with its description at the start of the same row from `lg`
     up. Below `lg` the breadcrumb names the page, so the description is hidden and a toolbar that
     holds nothing else is hidden with it. The negative margin pulls the block below up to a 12px
     gap inside the page column's 24px and 32px gaps, so the actions sit on that block's top corner. -->
<div
	class="-mb-3 flex flex-wrap items-center justify-end gap-2 lg:-mb-5 max-lg:[&:not(:has(>:not([data-description])))]:hidden {className}"
>
	{#if description}
		<p data-description class="hidden min-w-0 grow basis-80 text-sm text-surface-600-400 lg:block">
			{description}
		</p>
	{/if}
	{@render children?.()}
</div>
