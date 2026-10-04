<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import type { Snippet } from 'svelte';
	import { pageTitle } from '$lib/title.svelte';

	interface Props {
		title: string;
		/** One sentence on what the page holds. */
		description?: string;
		/** The list this page belongs to, shown as a link above the title. */
		back?: { href: string; label: string };
		/** The line under the title: status, identifiers, links. */
		children?: Snippet;
		/** The page's actions, on the title's right. The one filled primary button goes last. */
		actions?: Snippet;
	}

	let { title, description, back, children, actions }: Props = $props();

	$effect(() => {
		pageTitle.value = title;
	});
</script>

<!-- The title column and the actions share a row from `sm`; below that the actions drop under the title. -->
<header
	class="flex min-w-0 flex-col gap-3 sm:flex-row sm:items-start sm:justify-between sm:gap-x-6"
>
	<div class="min-w-0 space-y-2 sm:flex-1">
		{#if back}
			<a href={back.href} class="inline-flex items-center gap-1 anchor text-sm">
				<ArrowLeftIcon class="size-4" />
				{back.label}
			</a>
		{/if}
		<h1 class="h3 break-words">{title}</h1>
		{#if description}
			<p class="max-w-3xl text-surface-600-400">{description}</p>
		{/if}
		{@render children?.()}
	</div>
	{#if actions}
		<div class="flex flex-wrap items-center gap-2 sm:shrink-0">
			{@render actions()}
		</div>
	{/if}
</header>
