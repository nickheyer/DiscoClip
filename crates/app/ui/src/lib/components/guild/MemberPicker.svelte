<script lang="ts">
	import { Combobox, Portal, useListCollection } from '@skeletonlabs/skeleton-svelte';
	import { applications } from '$lib/api/endpoints';
	import type { GuildMember, Snowflake, Uuid } from '$lib/api/types';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import { reportError } from '$lib/toast.svelte';

	interface Props {
		applicationId: Uuid;
		guild: Snowflake;
		placeholder?: string;
		/** Members already chosen, left out of the results. */
		exclude?: Snowflake[];
		onpick: (member: GuildMember) => void;
	}

	let {
		applicationId,
		guild,
		placeholder = 'Search members',
		exclude = [],
		onpick
	}: Props = $props();

	let items = $state<GuildMember[]>([]);
	let searching = $state(false);
	let timer: ReturnType<typeof setTimeout> | null = null;
	let latest = 0;

	const collection = $derived(
		useListCollection({
			items: items.filter((member) => !exclude.includes(member.id)),
			itemToString: (member) => member.display_name ?? member.nick ?? member.username,
			itemToValue: (member) => member.id
		})
	);

	function search(q: string) {
		if (timer) clearTimeout(timer);
		const text = q.trim();
		if (!text) {
			items = [];
			return;
		}
		timer = setTimeout(async () => {
			const request = ++latest;
			searching = true;
			try {
				const found = await applications.members(applicationId, guild, text, 20);
				if (request === latest) items = found;
			} catch (error) {
				if (request === latest) reportError(error, 'Member search failed');
			} finally {
				if (request === latest) searching = false;
			}
		}, 250);
	}

	function label(member: GuildMember): string {
		const shown = member.display_name ?? member.nick ?? member.username;
		return shown === member.username ? shown : `${shown} (${member.username})`;
	}
</script>

<!-- Skeleton's Combobox searching the server as the person types. -->
<Combobox
	{collection}
	{placeholder}
	selectionBehavior="clear"
	inputBehavior="none"
	openOnClick
	onInputValueChange={(details) => search(details.inputValue)}
	onValueChange={(details) => {
		const member = details.items[0] as GuildMember | undefined;
		if (member) onpick(member);
	}}
>
	<Combobox.Control>
		<Combobox.Input aria-label={placeholder} />
		<Combobox.Trigger aria-label="Show members" />
	</Combobox.Control>
	<Portal>
		<Combobox.Positioner class="z-[60]">
			<Combobox.Content class="max-h-72 min-w-64 overflow-y-auto shadow-xl">
				{#if searching && items.length === 0}
					<p class="px-2 py-1 text-sm text-surface-600-400">Searching…</p>
				{:else if items.length === 0}
					<p class="px-2 py-1 text-sm text-surface-600-400">Type a name to search the server.</p>
				{/if}
				{#each collection.items as member (member.id)}
					<Combobox.Item item={member} class="justify-start gap-3">
						<DiscordAvatar user={member.id} hash={member.avatar} name={member.username} size={20} />
						<Combobox.ItemText class="flex-1">{label(member)}</Combobox.ItemText>
						{#if member.bot}<span class="text-xs text-surface-600-400">bot</span>{/if}
						<Combobox.ItemIndicator />
					</Combobox.Item>
				{/each}
			</Combobox.Content>
		</Combobox.Positioner>
	</Portal>
</Combobox>
