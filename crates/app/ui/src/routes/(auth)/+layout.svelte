<script lang="ts">
	import Wordmark from '$lib/brand/Wordmark.svelte';
	import { Toast } from '@skeletonlabs/skeleton-svelte';
	import { toaster } from '$lib/toast.svelte';

	let { children } = $props();
</script>

<div class="flex min-h-dvh flex-col items-center justify-center gap-8 p-6">
	<Wordmark size={36} />
	<main class="w-full max-w-md space-y-6 card bg-surface-100-900 p-6 shadow-lg sm:p-8">
		{@render children()}
	</main>
</div>

<Toast.Group {toaster}>
	{#snippet children(toast)}
		<Toast {toast}>
			<Toast.Message>
				<Toast.Title>{toast.title}</Toast.Title>
				<Toast.Description>{toast.description}</Toast.Description>
			</Toast.Message>
			<Toast.CloseTrigger />
		</Toast>
	{/snippet}
</Toast.Group>
