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

<!-- A Skeleton field group: the search icon, the input, and a clear button once there is text. -->
<div class="field-group grid-cols-[auto_1fr_auto] {className}">
	<span class="label label-text preset-tonal" aria-hidden="true"><SearchIcon class="size-4" /></span
	>
	<input
		{id}
		class="input"
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
		<button type="button" class="btn preset-tonal" onclick={clear} aria-label="Clear search">
			<XIcon class="size-4" />
		</button>
	{/if}
</div>
