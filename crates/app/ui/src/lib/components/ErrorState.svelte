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
	class="flex min-h-56 flex-col items-center justify-center gap-4 rounded-container border border-error-500/40 bg-error-50-950/40 px-6 py-10 text-center"
	role="alert"
>
	<TriangleAlertIcon class="size-6 text-error-700-300" />
	<p class="text-lg font-semibold">{title}</p>
	<p class="max-w-md text-sm leading-relaxed text-surface-700-300">{message}</p>
	{#if onretry}
		<button type="button" class="btn preset-tonal" onclick={onretry}>Retry</button>
	{/if}
</div>
