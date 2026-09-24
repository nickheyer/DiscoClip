<script lang="ts">
	import { Toast } from '@skeletonlabs/skeleton-svelte';
	import { toaster } from '$lib/toast.svelte';

	let { children } = $props();
</script>

<div class="min-h-dvh">
	{@render children()}
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
