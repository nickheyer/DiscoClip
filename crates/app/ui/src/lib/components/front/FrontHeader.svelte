<script lang="ts">
	import LogOutIcon from '@lucide/svelte/icons/log-out';
	import { resolve } from '$app/paths';
	import type { FrontInfo } from '$lib/api/types';
	import Mark from '$lib/brand/Mark.svelte';
	import ModeToggle from '$lib/components/ModeToggle.svelte';

	interface Props {
		info: FrontInfo;
		onlogout?: () => void;
	}

	let { info, onlogout }: Props = $props();
</script>

<header
	class="sticky top-0 z-30 border-b border-surface-200-800 bg-surface-50-950/90 backdrop-blur"
>
	<div class="mx-auto flex max-w-7xl items-center gap-4 px-4 py-3">
		<a
			href={resolve('/(front)/f/[slug]', { slug: info.slug })}
			class="flex min-w-0 items-center gap-3"
		>
			<Mark size={28} title="" class="shrink-0 text-primary-500" />
			<span class="min-w-0">
				<span class="block truncate font-semibold">{info.name}</span>
				{#if info.description}
					<span class="block truncate text-sm text-surface-600-400">{info.description}</span>
				{/if}
			</span>
		</a>
		<span class="flex-1"></span>
		{#if info.viewer}
			<span class="hidden truncate text-sm text-surface-600-400 sm:inline"
				>{info.viewer.display}</span
			>
			<button type="button" class="btn preset-tonal btn-sm" onclick={onlogout}>
				<LogOutIcon class="size-4" />
				<span class="hidden sm:inline">Log out</span>
			</button>
		{/if}
		<ModeToggle />
	</div>
</header>
