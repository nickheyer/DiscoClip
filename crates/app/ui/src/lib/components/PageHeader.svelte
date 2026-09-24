<script lang="ts">
	import type { Snippet } from 'svelte';
	import { pageTitle } from '$lib/title.svelte';

	interface Props {
		title: string;
		children?: Snippet;
	}

	let { title, children }: Props = $props();

	$effect(() => {
		pageTitle.value = title;
	});
</script>

<!-- Below `lg` the breadcrumb names the page, so the title is kept for assistive technology only
     and a header with nothing else leaves no box behind. -->
<header class="min-w-0 space-y-2 max-lg:[&:not(:has(>:not(h1)))]:contents">
	<h1 class="sr-only h2 text-3xl leading-tight break-words lg:not-sr-only">{title}</h1>
	{@render children?.()}
</header>
