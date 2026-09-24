<script lang="ts">
	import SearchIcon from '@lucide/svelte/icons/search';
	import XIcon from '@lucide/svelte/icons/x';
	import { onDestroy } from 'svelte';

	interface Props {
		id?: string;
		value?: string;
		placeholder?: string;
		/** Milliseconds of quiet before `onsearch` runs. */
		debounce?: number;
		onsearch?: (value: string) => void;
		class?: string;
	}

	let {
		id,
		value = $bindable(''),
		placeholder = 'Search',
		debounce = 250,
		onsearch,
		class: className = ''
	}: Props = $props();

	let timer: ReturnType<typeof setTimeout> | null = null;
	onDestroy(() => {
		if (timer) clearTimeout(timer);
	});

	function schedule() {
		if (timer) clearTimeout(timer);
		timer = setTimeout(() => {
			timer = null;
			onsearch?.(value);
		}, debounce);
	}

	function clear() {
		value = '';
		if (timer) clearTimeout(timer);
		timer = null;
		onsearch?.('');
	}
</script>

<div class="relative min-w-0 {className}">
	<SearchIcon
		class="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-surface-600-400"
	/>
	<input
		{id}
		class="input pr-12 pl-10"
		type="search"
		{placeholder}
		aria-label={id ? undefined : placeholder}
		bind:value
		oninput={schedule}
		onkeydown={(event) => {
			if (event.key === 'Enter') {
				if (timer) clearTimeout(timer);
				timer = null;
				onsearch?.(value);
			}
		}}
	/>
	{#if value}
		<button
			type="button"
			class="absolute top-1/2 right-1 btn-icon -translate-y-1/2 text-surface-600-400 btn-icon-sm hover:preset-tonal"
			onclick={clear}
			aria-label="Clear search"
		>
			<XIcon class="size-4" />
		</button>
	{/if}
</div>
