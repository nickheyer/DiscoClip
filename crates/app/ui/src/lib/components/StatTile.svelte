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
		primary: 'text-primary-700-300',
		success: 'text-success-800-200',
		warning: 'text-warning-800-200',
		error: 'text-error-700-300'
	};
</script>

<div
	class="flex min-w-0 flex-col gap-3 card border border-surface-200-800 bg-surface-100-900 p-5"
	aria-busy={loading}
>
	<p class="text-sm font-medium text-surface-700-300">{label}</p>
	{#if loading}
		<div class="h-10 placeholder w-20 animate-pulse" aria-hidden="true"></div>
		<div class="h-5 placeholder w-3/4 animate-pulse" aria-hidden="true"></div>
	{:else}
		<p
			class="text-3xl leading-none font-semibold tracking-tight break-words tabular-nums {TONE[
				tone
			]}"
		>
			{value}
		</p>
		{#if hint}
			<p class="mt-auto text-sm leading-relaxed text-surface-600-400">{hint}</p>
		{/if}
	{/if}
	{@render children?.()}
</div>
