<script lang="ts">
	import '../app.css';
	import { Toast } from '@skeletonlabs/skeleton-svelte';
	import { toaster } from '$lib/toast.svelte';

	let { children } = $props();
</script>

{@render children()}

<Toast.Group {toaster}>
	{#snippet children(toast)}
		<Toast {toast}>
			<Toast.Message>
				<Toast.Title>{toast.title}</Toast.Title>
				<Toast.Description>{toast.description}</Toast.Description>
			</Toast.Message>
			<Toast.CloseTrigger aria-label="Dismiss notification" />
		</Toast>
	{/snippet}
</Toast.Group>
