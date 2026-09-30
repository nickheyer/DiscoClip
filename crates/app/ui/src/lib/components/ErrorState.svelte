<script lang="ts">
	import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert';
	import { ApiError } from '$lib/api/client';

	interface Props {
		error: unknown;
		title?: string;
		onretry?: () => void;
	}

	let { error, title = 'This could not be loaded', onretry }: Props = $props();

	const message = $derived(
		error instanceof ApiError
			? error.message
			: error instanceof Error
				? error.message
				: String(error)
	);
</script>

<div
	class="flex min-h-40 flex-col items-center justify-center gap-3 card preset-tonal-error p-8 text-center"
	role="alert"
>
	<TriangleAlertIcon class="size-6" />
	<p class="font-semibold">{title}</p>
	<p class="max-w-md text-sm">{message}</p>
	{#if onretry}
		<button type="button" class="btn preset-filled-error-500" onclick={onretry}>Retry</button>
	{/if}
</div>
