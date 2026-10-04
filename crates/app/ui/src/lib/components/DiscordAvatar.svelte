<script lang="ts">
	import { Avatar } from '@skeletonlabs/skeleton-svelte';
	import type { Snowflake } from '$lib/api/types';
	import { discordAvatar, initials } from '$lib/format';

	interface Props {
		user: Snowflake;
		hash: string | null;
		name: string;
		/** CSS pixels. */
		size?: number;
		class?: string;
	}

	let { user, hash, name, size = 24, class: className = '' }: Props = $props();
</script>

<!-- Skeleton's Avatar: the member's picture, or their initials until it loads. -->
<Avatar
	class="shrink-0 preset-filled-surface-200-800 {className}"
	style="width: {size}px; height: {size}px; font-size: {Math.max(9, size * 0.38)}px"
	role="img"
	aria-label={name}
>
	<Avatar.Image
		src={discordAvatar(user, hash, size <= 32 ? 64 : 128)}
		alt=""
		class="h-full"
		loading="lazy"
	/>
	<Avatar.Fallback class="font-semibold">{initials(name)}</Avatar.Fallback>
</Avatar>
