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

<div class="flex flex-wrap items-center justify-between gap-4 text-sm">
	<p class="text-surface-600-400">
		{#if total === 0}
			No results
		{:else}
			{number(first)}–{number(last)} of {number(total)}
		{/if}
	</p>
	{#if total > limit}
		<Pagination
			count={total}
			pageSize={limit}
			{page}
			siblingCount={1}
			onPageChange={(details) => {
				offset = (details.page - 1) * limit;
				onchange?.(offset);
			}}
		>
			<Pagination.PrevTrigger aria-label="Previous page">
				<ChevronLeftIcon class="size-4" />
			</Pagination.PrevTrigger>
			<Pagination.Context>
				{#snippet children(pagination)}
					{#each pagination().pages as item, index (index)}
						{#if item.type === 'page'}
							<Pagination.Item {...item}>{item.value}</Pagination.Item>
						{:else}
							<Pagination.Ellipsis {index}>…</Pagination.Ellipsis>
						{/if}
					{/each}
				{/snippet}
			</Pagination.Context>
			<Pagination.NextTrigger aria-label="Next page">
				<ChevronRightIcon class="size-4" />
			</Pagination.NextTrigger>
		</Pagination>
	{/if}
</div>
