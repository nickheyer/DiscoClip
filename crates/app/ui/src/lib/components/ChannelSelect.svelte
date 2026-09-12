<script lang="ts">
	import type { GuildChannel } from '$lib/api';
	import { channelOptionLabel, groupChannels, watchable } from '$lib/discord';

	interface Props {
		id: string;
		value: string;
		/** The guild's channels as the bot lists them; only message channels are offered. */
		channels: GuildChannel[];
		/** What choosing nothing means; without it a channel has to be chosen. */
		emptyLabel?: string;
		/** Channels listed but not offered, since a rule already watches each. */
		taken?: Set<string>;
		disabled?: boolean;
		invalid?: boolean;
		/** Called with the chosen channel's id, or `''` for the empty choice. */
		onchange?: (value: string) => void;
	}

	let {
		id,
		value = $bindable(''),
		channels,
		emptyLabel,
		taken = new Set(),
		disabled = false,
		invalid = false,
		onchange
	}: Props = $props();

	function onChange(event: Event) {
		value = (event.currentTarget as HTMLSelectElement).value;
		onchange?.(value);
	}

	const groups = $derived(
		groupChannels(channels)
			.map((group) => ({ ...group, channels: group.channels.filter((c) => watchable(c.kind)) }))
			.filter((group) => group.channels.length > 0)
	);
	/** The value when it names no listed channel: a thread, or a channel since deleted. */
	const unlisted = $derived(value !== '' && !channels.some((c) => c.id === value) ? value : null);
</script>

<select {id} class="select" {value} onchange={onChange} {disabled} aria-invalid={invalid ? 'true' : undefined}>
	{#if emptyLabel !== undefined}
		<option value="">{emptyLabel}</option>
	{:else if value === ''}
		<option value="" disabled>Choose a channel</option>
	{/if}
	{#if unlisted}
		<option value={unlisted}>Channel {unlisted} (not in the list)</option>
	{/if}
	{#each groups as group, i (group.category?.id ?? `loose-${i}`)}
		{#if group.category}
			<optgroup label={group.category.name}>
				{#each group.channels as channel (channel.id)}
					{@render option(channel)}
				{/each}
			</optgroup>
		{:else}
			{#each group.channels as channel (channel.id)}
				{@render option(channel)}
			{/each}
		{/if}
	{/each}
</select>

{#snippet option(channel: GuildChannel)}
	<option value={channel.id} disabled={taken.has(channel.id)}>
		{channelOptionLabel(channel)}{taken.has(channel.id) ? ' — already watched' : ''}
	</option>
{/snippet}
