<script lang="ts">
	import type { Snowflake } from '$lib/api/types';
	import { discordAvatar } from '$lib/format';

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

<img
	src={discordAvatar(user, hash, size <= 32 ? 64 : 128)}
	alt={name}
	width={size}
	height={size}
	class="shrink-0 rounded-full bg-surface-200-800 {className}"
	loading="lazy"
/>
