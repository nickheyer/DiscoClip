<script lang="ts" module>
	import type { Snippet } from 'svelte';

	export interface Column<T> {
		key: string;
		label: string;
		/** Keep the label for assistive technology only, as on an actions column. */
		hideLabel?: boolean;
		sortable?: boolean;
		/** Classes for the body cells of this column. */
		class?: string;
		/** Classes for the header cell, such as a width. */
		headerClass?: string;
		align?: 'left' | 'right' | 'center';
		/** The plain value, for text cells and local sorting. */
		value?: (row: T) => string | number | null | undefined;
		/** Custom cell content. */
		cell?: Snippet<[T]>;
		/** Leave the column out when no row has a value for it. */
		optional?: boolean;
	}

	export type SortDir = 'asc' | 'desc';
</script>

<script lang="ts" generics="T">
	import ArrowDownIcon from '@lucide/svelte/icons/arrow-down';
	import ArrowUpIcon from '@lucide/svelte/icons/arrow-up';
	import { EMPTY } from '$lib/format';

	interface Props {
		rows: T[];
		columns: Column<T>[];
		rowKey: (row: T) => string;
		loading?: boolean;
		placeholderRows?: number;
		/** Shown when there are no rows and nothing is loading. */
		empty?: Snippet;
		sortKey?: string | null;
		sortDir?: SortDir;
		/** Sorting the parent performs, such as on the server. Without it, rows sort here. */
		onsort?: (key: string | null, dir: SortDir) => void;
		selectable?: boolean;
		selected?: string[];
		/** Destination of the row's button. */
		rowHref?: (row: T) => string;
		/** Label of that button: Edit, or View for rows that open a read-only page. */
		rowLabel?: string;
		rowClass?: (row: T) => string;
		/** Drop the card around the table: it sits inside a card already. */
		flush?: boolean;
		class?: string;
	}

	let {
		rows,
		columns,
		rowKey,
		loading = false,
		placeholderRows = 5,
		empty,
		sortKey = $bindable(null),
		sortDir = $bindable('asc'),
		onsort,
		selectable = false,
		selected = $bindable([]),
		rowHref,
		rowLabel = 'Edit',
		rowClass,
		flush = false,
		class: className = ''
	}: Props = $props();

	const ALIGN = { left: 'text-left', right: 'text-right', center: 'text-center' };

	const blank = (v: string | number | null | undefined) =>
		v === null || v === undefined || v === '';

	/** The columns with something to show: optional ones go when every row is blank in them. */
	const shownColumns = $derived(
		rows.length === 0
			? columns
			: columns.filter(
					(column) =>
						!column.optional || !column.value || rows.some((row) => !blank(column.value!(row)))
				)
	);

	const shown = $derived.by(() => {
		if (onsort || !sortKey) return rows;
		const column = columns.find((c) => c.key === sortKey);
		if (!column?.value) return rows;
		const value = column.value;
		const direction = sortDir === 'asc' ? 1 : -1;
		return [...rows].sort((a, b) => {
			const va = value(a);
			const vb = value(b);
			if (va === vb) return 0;
			if (va === null || va === undefined) return 1;
			if (vb === null || vb === undefined) return -1;
			if (typeof va === 'number' && typeof vb === 'number') return (va - vb) * direction;
			return String(va).localeCompare(String(vb), undefined, { numeric: true }) * direction;
		});
	});

	function sortBy(column: Column<T>) {
		if (!column.sortable) return;
		if (sortKey !== column.key) {
			sortKey = column.key;
			sortDir = 'asc';
		} else if (sortDir === 'asc') {
			sortDir = 'desc';
		} else {
			sortKey = null;
			sortDir = 'asc';
		}
		onsort?.(sortKey, sortDir);
	}

	const keys = $derived(shown.map(rowKey));
	const allSelected = $derived(keys.length > 0 && keys.every((k) => selected.includes(k)));
	const someSelected = $derived(!allSelected && keys.some((k) => selected.includes(k)));

	function toggleAll() {
		selected = allSelected
			? selected.filter((k) => !keys.includes(k))
			: [...new Set([...selected, ...keys])];
	}

	function toggle(key: string) {
		selected = selected.includes(key) ? selected.filter((k) => k !== key) : [...selected, key];
	}

	const colSpan = $derived(shownColumns.length + (selectable ? 1 : 0) + (rowHref ? 1 : 0));
</script>

<!-- Skeleton's table inside its table-wrap, with the row hover the docs use. -->
<div
	class="table-wrap {flush ? '' : 'card preset-filled-surface-100-900 p-2'} {className}"
	aria-busy={loading}
>
	<table class="table">
		<thead>
			<tr>
				{#if selectable}
					<th class="w-9">
						<input
							class="checkbox"
							type="checkbox"
							checked={allSelected}
							indeterminate={someSelected}
							onchange={toggleAll}
							aria-label="Select all rows"
							disabled={keys.length === 0}
						/>
					</th>
				{/if}
				{#each shownColumns as column (column.key)}
					<th
						class="{ALIGN[column.align ?? 'left']} {column.headerClass ?? ''} whitespace-nowrap"
						scope="col"
						aria-sort={sortKey === column.key
							? sortDir === 'asc'
								? 'ascending'
								: 'descending'
							: undefined}
					>
						{#if column.sortable}
							<button
								type="button"
								class="inline-flex items-center gap-1 hover:text-surface-950-50"
								onclick={() => sortBy(column)}
							>
								{column.label}
								{#if sortKey === column.key}
									{#if sortDir === 'asc'}
										<ArrowUpIcon class="size-3.5" />
									{:else}
										<ArrowDownIcon class="size-3.5" />
									{/if}
								{/if}
							</button>
						{:else if column.hideLabel}
							<span class="sr-only">{column.label}</span>
						{:else}
							{column.label}
						{/if}
					</th>
				{/each}
				{#if rowHref}
					<th scope="col" class="w-20 text-right"><span class="sr-only">{rowLabel}</span></th>
				{/if}
			</tr>
		</thead>
		<tbody class="[&>tr]:hover:preset-tonal">
			{#if loading && rows.length === 0}
				{#each { length: placeholderRows }, i (i)}
					<tr aria-hidden="true">
						{#if selectable}
							<td><div class="size-4 placeholder animate-pulse"></div></td>
						{/if}
						{#each shownColumns as column (column.key)}
							<td class={column.class ?? ''}>
								<div
									class="h-4 placeholder animate-pulse"
									style="width: {45 + ((i * 17 + column.key.length * 13) % 45)}%"
								></div>
							</td>
						{/each}
						{#if rowHref}<td><div class="ml-auto h-7 placeholder w-14 animate-pulse"></div></td
							>{/if}
					</tr>
				{/each}
			{:else if rows.length === 0}
				<tr>
					<td colspan={colSpan} class="py-10 text-center text-surface-600-400">
						{#if empty}
							{@render empty()}
						{:else}
							Nothing here yet.
						{/if}
					</td>
				</tr>
			{:else}
				{#each shown as row (rowKey(row))}
					{@const key = rowKey(row)}
					<tr
						class="{selected.includes(key) ? 'preset-tonal-primary' : ''} {rowClass?.(row) ?? ''}"
					>
						{#if selectable}
							<td>
								<input
									class="checkbox"
									type="checkbox"
									checked={selected.includes(key)}
									onchange={() => toggle(key)}
									aria-label="Select row"
								/>
							</td>
						{/if}
						{#each shownColumns as column (column.key)}
							<td class="{ALIGN[column.align ?? 'left']} {column.class ?? ''}">
								{#if column.cell}
									{@render column.cell(row)}
								{:else if column.value}
									{@const v = column.value(row)}
									{#if blank(v)}
										<span class="text-surface-600-400">{EMPTY}</span>
									{:else}
										{v}
									{/if}
								{/if}
							</td>
						{/each}
						{#if rowHref}
							<td class="text-right">
								<a
									href={rowHref(row)}
									class="btn preset-tonal btn-sm"
									aria-label="{rowLabel} {columns[0]?.value?.(row) ?? rowKey(row)}">{rowLabel}</a
								>
							</td>
						{/if}
					</tr>
				{/each}
			{/if}
		</tbody>
	</table>
</div>
