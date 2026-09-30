<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		label: string;
		value: string | number;
		/** A second line under the value. */
		hint?: string;
		tone?: 'surface' | 'primary' | 'success' | 'warning' | 'error';
		children?: Snippet;
		loading?: boolean;
	}

	let { label, value, hint, tone = 'surface', children, loading = false }: Props = $props();

	const TONE = {
		surface: '',
		primary: 'text-primary-600-400',
		success: 'text-success-600-400',
		warning: 'text-warning-600-400',
		error: 'text-error-600-400'
	};
</script>

<div class="flex min-w-0 flex-col gap-1 card preset-filled-surface-100-900 p-4" aria-busy={loading}>
	<p class="text-sm font-medium text-surface-600-400">{label}</p>
	{#if loading}
		<div class="h-8 placeholder w-16 animate-pulse" aria-hidden="true"></div>
		<div class="h-4 placeholder w-3/4 animate-pulse" aria-hidden="true"></div>
	{:else}
		<p class="text-2xl font-semibold tracking-tight break-words tabular-nums {TONE[tone]}">
			{value}
		</p>
		{#if hint}
			<p class="mt-auto text-xs break-words text-surface-600-400">{hint}</p>
		{/if}
	{/if}
	{@render children?.()}
</div>
