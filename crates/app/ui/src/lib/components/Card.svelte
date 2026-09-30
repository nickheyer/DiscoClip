<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		/** The card's heading. Without one the body fills the card. */
		title?: string;
		/** One sentence under the heading. */
		description?: string;
		/** A count or state beside the heading. */
		count?: string | number;
		/** Controls on the heading's right. */
		actions?: Snippet;
		children: Snippet;
		/** Drop the body padding, for a table or a list that runs edge to edge. */
		flush?: boolean;
		class?: string;
		/** The element's own label for assistive technology, when the title is not it. */
		label?: string;
	}

	let {
		title,
		description,
		count,
		actions,
		children,
		flush = false,
		class: className = '',
		label
	}: Props = $props();

	const id = $props.id();
</script>

<section
	class="flex min-w-0 flex-col card preset-filled-surface-100-900 {className}"
	aria-labelledby={title && !label ? `${id}-title` : undefined}
	aria-label={label}
>
	{#if title || actions}
		<header
			class="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 p-4 {flush
				? 'border-b border-surface-200-800'
				: 'pb-0'}"
		>
			<div class="min-w-0 space-y-1">
				{#if title}
					<h2 id="{id}-title" class="flex flex-wrap items-center gap-2 h6">
						{title}
						{#if count !== undefined && count !== null}
							<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">{count}</span>
						{/if}
					</h2>
				{/if}
				{#if description}
					<p class="text-sm text-surface-600-400">{description}</p>
				{/if}
			</div>
			{#if actions}
				<div class="flex flex-wrap items-center gap-2">{@render actions()}</div>
			{/if}
		</header>
	{/if}
	<div class="min-w-0 flex-1 {flush ? '' : 'p-4'}">
		{@render children()}
	</div>
</section>
