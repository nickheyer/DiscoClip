<script lang="ts">
	import MoonIcon from '@lucide/svelte/icons/moon';
	import SunIcon from '@lucide/svelte/icons/sun';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { mode } from '$lib/mode.svelte';
</script>

<!-- Skeleton's Switch as the light switch: on is dark mode, with the icon on the thumb. -->
<Switch
	checked={mode.current === 'dark'}
	onCheckedChange={() => mode.toggle()}
	aria-label={mode.action}
	title={mode.action}
>
	<Switch.Control>
		<Switch.Thumb>
			{#if mode.current === 'dark'}
				<MoonIcon class="size-3" />
			{:else}
				<SunIcon class="size-3" />
			{/if}
		</Switch.Thumb>
	</Switch.Control>
	<Switch.HiddenInput />
</Switch>
