<script lang="ts">
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import Wordmark from '$lib/brand/Wordmark.svelte';

	const title = $derived(
		page.status === 404
			? 'There is nothing here'
			: page.status === 403
				? 'You are not allowed here'
				: 'Something went wrong'
	);
</script>

<svelte:head>
	<title>{page.status} · DiscoClip</title>
</svelte:head>

<main class="flex min-h-dvh flex-col items-center justify-center gap-6 p-6 text-center">
	<Wordmark size={36} />
	<div class="space-y-2">
		<p class="text-6xl font-bold text-primary-500 tabular-nums">{page.status}</p>
		<h1 class="h4">{title}</h1>
		{#if page.error?.message && page.status !== 404}
			<p class="max-w-md text-sm text-surface-600-400">{page.error.message}</p>
		{/if}
	</div>
	<div class="flex gap-2">
		<a href={resolve('/')} class="btn preset-filled-primary-500">Dashboard</a>
		<button type="button" class="btn preset-tonal" onclick={() => history.back()}>Go back</button>
	</div>
</main>
