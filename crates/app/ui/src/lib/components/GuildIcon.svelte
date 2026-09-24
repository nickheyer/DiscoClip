<script lang="ts">
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

{#if src}
	<img
		{src}
		alt={name}
		width={size}
		height={size}
		class="shrink-0 rounded-full bg-surface-200-800 {className}"
		loading="lazy"
	/>
{:else}
	<span
		class="inline-flex shrink-0 items-center justify-center rounded-full bg-surface-200-800 font-semibold text-surface-700-300 {className}"
		style="width: {size}px; height: {size}px; font-size: {Math.max(10, size * 0.38)}px"
		role="img"
		aria-label={name}
	>
		{initials(name)}
	</span>
{/if}
