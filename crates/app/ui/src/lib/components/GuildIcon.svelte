<script lang="ts">
	import { Avatar } from '@skeletonlabs/skeleton-svelte';
	import type { Snowflake } from '$lib/api/types';
	import { discordIcon, initials } from '$lib/format';

	interface Props {
		guild: Snowflake;
		hash: string | null;
		name: string;
		/** CSS pixels. */
		size?: number;
		class?: string;
	}

	let { guild, hash, name, size = 32, class: className = '' }: Props = $props();

	const src = $derived(discordIcon(guild, hash, size <= 32 ? 64 : 128));
</script>

<!-- Skeleton's Avatar: the server's icon, or its initials while there is none. -->
<Avatar
	class="shrink-0 preset-filled-surface-200-800 {className}"
	style="width: {size}px; height: {size}px; font-size: {Math.max(10, size * 0.38)}px"
	role="img"
	aria-label={name}
>
	{#if src}
		<Avatar.Image {src} alt="" class="h-full" loading="lazy" />
	{/if}
	<Avatar.Fallback class="font-semibold">{initials(name)}</Avatar.Fallback>
</Avatar>
