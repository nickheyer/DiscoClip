<script lang="ts">
	import MoonIcon from '@lucide/svelte/icons/moon';
	import SunIcon from '@lucide/svelte/icons/sun';
	import { mode } from '$lib/mode.svelte';
</script>

<button
	type="button"
	class="btn min-w-11 gap-2 hover:preset-tonal"
	onclick={() => mode.toggle()}
	title={mode.action}
	aria-label={mode.action}
>
	{#if mode.current === 'dark'}
		<SunIcon class="size-5" />
	{:else}
		<MoonIcon class="size-5" />
	{/if}
	<span class="hidden lg:inline">{mode.label}</span>
</button>
