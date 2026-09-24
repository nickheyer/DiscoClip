<script lang="ts">
	import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left';
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import { Pagination } from '@skeletonlabs/skeleton-svelte';
	import { number } from '$lib/format';

	interface Props {
		total: number;
		limit: number;
		offset?: number;
		onchange?: (offset: number) => void;
	}

	let { total, limit, offset = $bindable(0), onchange }: Props = $props();

	const page = $derived(Math.floor(offset / Math.max(1, limit)) + 1);
	const first = $derived(total === 0 ? 0 : offset + 1);
	const last = $derived(Math.min(total, offset + limit));
</script>

<div class="flex flex-wrap items-center justify-between gap-4 py-1 text-sm">
	<p class="text-surface-600-400">
		{#if total === 0}
			No results
		{:else}
			{number(first)}–{number(last)} of {number(total)}
		{/if}
	</p>
	{#if total > limit}
		<Pagination
			class="max-w-full flex-wrap gap-1 p-1.5"
			count={total}
			pageSize={limit}
			{page}
			siblingCount={1}
			onPageChange={(details) => {
				offset = (details.page - 1) * limit;
				onchange?.(offset);
			}}
		>
			<Pagination.PrevTrigger class="min-h-10 min-w-10" aria-label="Previous page">
				<ChevronLeftIcon class="size-4" />
			</Pagination.PrevTrigger>
			<Pagination.Context>
				{#snippet children(pagination)}
					{#each pagination().pages as item, index (index)}
						{#if item.type === 'page'}
							<Pagination.Item
								{...item}
								class="min-h-10 min-w-10 {item.value === page ? '' : 'max-sm:hidden'}"
								>{item.value}</Pagination.Item
							>
						{:else}
							<Pagination.Ellipsis {index} class="hidden min-h-10 min-w-8 sm:inline-flex"
								>…</Pagination.Ellipsis
							>
						{/if}
					{/each}
				{/snippet}
			</Pagination.Context>
			<Pagination.NextTrigger class="min-h-10 min-w-10" aria-label="Next page">
				<ChevronRightIcon class="size-4" />
			</Pagination.NextTrigger>
		</Pagination>
	{/if}
</div>
