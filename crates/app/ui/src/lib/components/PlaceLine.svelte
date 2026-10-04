<script lang="ts">
	import ArrowRightIcon from '@lucide/svelte/icons/arrow-right';
	import type { Origin, Place } from '$lib/api/types';
	import ChannelKindIcon from './guild/ChannelKindIcon.svelte';
	import DiscordAvatar from './DiscordAvatar.svelte';
	import GuildIcon from './GuildIcon.svelte';

	interface Props {
		origin: Origin;
		place: Place | null;
		/** Where the result goes when a rule sends it to another channel, as an id. */
		destination?: string | null;
		/** Who submitted, as the request records it: `discord:<id>`, a username, or nothing. */
		submittedBy?: string | null;
		/** Leave the author out, for a table cell. */
		compact?: boolean;
		class?: string;
	}

	let {
		origin,
		place,
		destination = null,
		submittedBy = null,
		compact = false,
		class: className = ''
	}: Props = $props();

	const discord = $derived(origin.source === 'discord');
	const author = $derived(place?.author ?? null);
	const authorName = $derived(
		author ? (author.display_name ?? author.nick ?? author.username) : null
	);
	/** The submitter as an id, when the request came from Discord and the bot has not seen them. */
	const submitterId = $derived(
		submittedBy?.startsWith('discord:') ? submittedBy.slice('discord:'.length) : submittedBy
	);
</script>

{#if !discord}
	<span class="inline-flex items-center gap-1.5 {className}">
		<span class="capitalize">{origin.source}</span>
		{#if submittedBy}<span class="text-surface-600-400">· {submittedBy}</span>{/if}
	</span>
{:else}
	<span class="inline-flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-1 {className}">
		{#if place?.guild}
			<GuildIcon guild={place.guild.id} hash={place.guild.icon} name={place.guild.name} size={20} />
			<span class="truncate font-medium">{place.guild.name}</span>
		{:else if origin.guild}
			<span class="text-surface-600-400">Server</span>
			<span class="font-mono text-xs">{origin.guild}</span>
		{:else}
			<span class="font-medium">Direct message</span>
		{/if}
		{#if place?.channel}
			<span class="inline-flex items-center gap-0.5 text-surface-700-300">
				<ChannelKindIcon kind={place.channel.kind} class="size-3.5 text-surface-600-400" />
				{place.channel.name}
			</span>
		{:else if origin.channel}
			<span class="text-surface-600-400">channel</span>
			<span class="font-mono text-xs">{origin.channel}</span>
		{/if}
		{#if place?.destination}
			<ArrowRightIcon class="size-3.5 text-surface-600-400" aria-label="posts to" />
			<span class="inline-flex items-center gap-0.5 text-surface-700-300">
				<ChannelKindIcon kind={place.destination.kind} class="size-3.5 text-surface-600-400" />
				{place.destination.name}
			</span>
		{:else if destination && destination !== origin.channel}
			<ArrowRightIcon class="size-3.5 text-surface-600-400" aria-label="posts to" />
			<span class="text-surface-600-400">channel</span>
			<span class="font-mono text-xs">{destination}</span>
		{/if}
		{#if !compact}
			{#if author && authorName}
				<span class="text-surface-600-400">by</span>
				<DiscordAvatar user={author.id} hash={author.avatar} name={author.username} size={20} />
				<span class="truncate">{authorName}</span>
			{:else if submitterId}
				<span class="text-surface-600-400">by member</span>
				<span class="font-mono text-xs">{submitterId}</span>
			{/if}
		{/if}
	</span>
{/if}
