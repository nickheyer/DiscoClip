<script lang="ts">
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import { SvelteSet } from 'svelte/reactivity';
	import { applications } from '$lib/api/endpoints';
	import type { BotGuild, GuildChannel, Snowflake, Uuid } from '$lib/api/types';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { number } from '$lib/format';
	import ChannelKindIcon from './ChannelKindIcon.svelte';
	import { channelLabel, watchable } from './channels';

	interface Props {
		/** The servers the bots are in, each with the application whose bot lists its channels. */
		servers: { guild: BotGuild; applicationId: Uuid }[];
		/** Servers taken whole: every channel in them, including ones made later. */
		guilds: Snowflake[];
		/** Channels taken on their own, of servers not taken whole. */
		channels: Snowflake[];
	}

	let { servers, guilds = $bindable(), channels = $bindable() }: Props = $props();

	/** One server row: one a bot is in, or an id kept from an earlier save. */
	interface ServerRow {
		id: Snowflake;
		name: string;
		icon: string | null;
		present: boolean;
		/** The application whose bot lists the channels, or nothing for a server no bot is in. */
		applicationId: Uuid | null;
	}

	let channelsByGuild = $state<Record<Snowflake, GuildChannel[]>>({});
	const loading = new SvelteSet<Snowflake>();
	let failed = $state<Record<Snowflake, string>>({});
	const expanded = new SvelteSet<Snowflake>();

	const rows = $derived.by((): ServerRow[] => {
		const known = servers.map(({ guild, applicationId }): ServerRow => ({
			id: guild.guild_id,
			name: guild.name,
			icon: guild.icon,
			present: guild.present,
			applicationId
		}));
		const ids = new Set(known.map((row) => row.id));
		const unknown = guilds
			.filter((guild) => !ids.has(guild))
			.map((guild): ServerRow => ({
				id: guild,
				name: guild,
				icon: null,
				present: false,
				applicationId: null
			}));
		return [...known, ...unknown];
	});

	/** The channels a server offers, in tree order with their category. */
	function listOf(guild: Snowflake) {
		return watchable(channelsByGuild[guild] ?? []);
	}

	/** Every channel id a loaded list names. */
	const placed = $derived(
		new Set(Object.values(channelsByGuild).flatMap((list) => list.map((c) => c.id)))
	);
	/** Channels picked that no loaded list names yet. */
	const unplaced = $derived(channels.filter((channel) => !placed.has(channel)));
	/** Whether every server that can list its channels has answered, one way or the other. */
	const settled = $derived(
		loading.size === 0 &&
			rows.every((row) => !row.applicationId || channelsByGuild[row.id] || failed[row.id])
	);
	/** Channels picked that no bot lists, once every list has been asked for. */
	const orphans = $derived(settled ? unplaced : []);

	async function load(row: ServerRow) {
		if (!row.applicationId || channelsByGuild[row.id] || loading.has(row.id)) return;
		loading.add(row.id);
		const next = { ...failed };
		delete next[row.id];
		failed = next;
		try {
			const list = await applications.channels(row.applicationId, row.id);
			channelsByGuild = { ...channelsByGuild, [row.id]: list };
			// A server taken whole already covers its channels, so none of them is listed twice.
			if (guilds.includes(row.id)) {
				channels = without(channels, new Set(watchable(list).map(({ channel }) => channel.id)));
			}
		} catch (err) {
			failed = { ...failed, [row.id]: err instanceof Error ? err.message : String(err) };
		} finally {
			loading.delete(row.id);
		}
	}

	// A saved scope names channels without their servers. Every server's list is asked for
	// until each channel sits under its server.
	$effect(() => {
		if (unplaced.length === 0) return;
		for (const row of rows) {
			if (row.applicationId && !channelsByGuild[row.id] && !failed[row.id]) void load(row);
		}
	});

	function open(row: ServerRow) {
		if (expanded.has(row.id)) expanded.delete(row.id);
		else {
			expanded.add(row.id);
			void load(row);
		}
	}

	const without = (list: Snowflake[], drop: Set<Snowflake>) => list.filter((id) => !drop.has(id));

	function toggleServer(row: ServerRow) {
		const own = new Set(listOf(row.id).map(({ channel }) => channel.id));
		if (guilds.includes(row.id)) {
			guilds = guilds.filter((id) => id !== row.id);
			channels = without(channels, own);
			return;
		}
		guilds = [...guilds, row.id];
		channels = without(channels, own);
		expanded.add(row.id);
		void load(row);
	}

	function toggleChannel(row: ServerRow, channelId: Snowflake) {
		const own = listOf(row.id).map(({ channel }) => channel.id);
		if (guilds.includes(row.id)) {
			guilds = guilds.filter((id) => id !== row.id);
			channels = [...new Set([...channels, ...own])].filter((id) => id !== channelId);
			return;
		}
		if (channels.includes(channelId)) {
			channels = channels.filter((id) => id !== channelId);
			return;
		}
		const next = [...channels, channelId];
		if (own.every((id) => next.includes(id))) {
			guilds = [...guilds, row.id];
			channels = without(next, new Set(own));
		} else {
			channels = next;
		}
	}

	function dropOrphan(channelId: Snowflake) {
		channels = channels.filter((id) => id !== channelId);
	}

	/** How much of a server is taken, beside its name. */
	function summary(row: ServerRow): string {
		if (guilds.includes(row.id)) return 'every channel';
		const list = listOf(row.id);
		const picked = list.filter(({ channel }) => channels.includes(channel.id)).length;
		if (picked === 0) return '';
		return `${number(picked)} of ${number(list.length)} channels`;
	}
</script>

<!-- Skeleton's table: one row per server, its channels beneath when opened, a checkbox on each. -->
<div class="max-h-[28rem] table-wrap overflow-y-auto">
	<table class="table">
		<thead class="sticky top-0 z-10 bg-surface-100-900">
			<tr>
				<th scope="col" class="w-9"><span class="sr-only">Included</span></th>
				<th scope="col">Server</th>
			</tr>
		</thead>
		<tbody>
			{#if rows.length === 0}
				<tr>
					<td colspan="2" class="py-8 text-center text-sm text-surface-600-400">
						No bot is in a server yet.
					</td>
				</tr>
			{/if}
			{#each rows as row (row.id)}
				{@const whole = guilds.includes(row.id)}
				{@const list = listOf(row.id)}
				{@const picked = list.filter(({ channel }) => channels.includes(channel.id)).length}
				{@const isOpen = expanded.has(row.id)}
				<tr class="hover:preset-tonal">
					<td>
						<input
							class="checkbox"
							type="checkbox"
							checked={whole}
							indeterminate={!whole && picked > 0}
							onchange={() => toggleServer(row)}
							aria-label="Include {row.name}"
						/>
					</td>
					<td>
						{#if row.applicationId}
							<button
								type="button"
								class="flex min-w-0 items-center gap-2 text-left"
								onclick={() => open(row)}
								aria-expanded={isOpen}
								aria-label="{isOpen ? 'Hide' : 'Show'} the channels of {row.name}"
							>
								<ChevronRightIcon
									class="size-4 shrink-0 text-surface-600-400 transition-transform {isOpen
										? 'rotate-90'
										: ''}"
								/>
								<GuildIcon guild={row.id} hash={row.icon} name={row.name} size={24} />
								<span class="truncate font-medium">{row.name}</span>
								{#if !row.present}<span class="text-xs text-surface-600-400">left</span>{/if}
								{#if summary(row)}
									<span class="text-xs text-surface-600-400">{summary(row)}</span>
								{/if}
							</button>
						{:else}
							<span class="flex min-w-0 items-center gap-2">
								<span class="size-4 shrink-0" aria-hidden="true"></span>
								<span class="font-mono text-xs">{row.id}</span>
								<span class="text-xs text-surface-600-400">no bot is in it</span>
							</span>
						{/if}
					</td>
				</tr>
				{#if isOpen}
					{#if loading.has(row.id)}
						<tr>
							<td></td>
							<td class="pl-8 text-sm text-surface-600-400">
								<span class="inline-flex items-center gap-2"><Spinner /> Listing channels…</span>
							</td>
						</tr>
					{:else if failed[row.id]}
						<tr>
							<td></td>
							<td class="pl-8 text-sm">
								<span class="text-error-600-400"
									>Channels could not be listed. {failed[row.id]}</span
								>
								<button
									type="button"
									class="ml-2 btn preset-tonal btn-sm"
									onclick={() => {
										const next = { ...failed };
										delete next[row.id];
										failed = next;
										void load(row);
									}}
								>
									Try again
								</button>
							</td>
						</tr>
					{:else if list.length === 0}
						<tr>
							<td></td>
							<td class="pl-8 text-sm text-surface-600-400"> The bot sees no channels here. </td>
						</tr>
					{:else}
						{#each list as { channel, category } (channel.id)}
							{@const included = whole || channels.includes(channel.id)}
							<tr class="hover:preset-tonal">
								<td>
									<input
										class="checkbox"
										type="checkbox"
										checked={included}
										onchange={() => toggleChannel(row, channel.id)}
										aria-label="Include {channelLabel(channel)} in {row.name}"
									/>
								</td>
								<td>
									<span class="flex min-w-0 items-center gap-2 pl-8 text-sm">
										<ChannelKindIcon
											kind={channel.kind}
											class="size-4 shrink-0 text-surface-600-400"
										/>
										<span class="truncate {included ? '' : 'text-surface-600-400'}">
											{channelLabel(channel)}
										</span>
										{#if category}
											<span class="truncate text-xs text-surface-600-400">{category}</span>
										{/if}
									</span>
								</td>
							</tr>
						{/each}
					{/if}
				{/if}
			{/each}
			{#if orphans.length > 0}
				<tr>
					<td></td>
					<td class="pt-3 text-xs font-semibold tracking-wide text-surface-600-400 uppercase">
						Channels no bot lists right now
					</td>
				</tr>
				{#each orphans as channelId (channelId)}
					<tr class="hover:preset-tonal">
						<td>
							<input
								class="checkbox"
								type="checkbox"
								checked
								onchange={() => dropOrphan(channelId)}
								aria-label="Include channel {channelId}"
							/>
						</td>
						<td><span class="pl-8 font-mono text-xs">{channelId}</span></td>
					</tr>
				{/each}
			{/if}
		</tbody>
	</table>
</div>
