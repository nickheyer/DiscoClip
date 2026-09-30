<script lang="ts">
	import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import type { Assignment, GuildChannel, Profile, Rule, Snowflake, Uuid } from '$lib/api/types';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { number } from '$lib/format';
	import ChannelKindIcon from './ChannelKindIcon.svelte';
	import { channelTree, type ChannelNode } from './channels';
	import { ruleSummary } from './watching';

	interface Props {
		channels: GuildChannel[];
		/** The server's rules: one per channel with its own, and the server's own without a channel. */
		rules: Rule[];
		profiles: Profile[];
		/** The channel-scoped assignments of this server. */
		assignments: Assignment[];
		/** The name of the profile a channel without one of its own gets. */
		inherited: string;
		/** Starts or stops watching a channel. Rejects when the server refuses. */
		onwatch: (channel: Snowflake, on: boolean) => Promise<void>;
		/** Opens the options of a watched channel: its own rule, or none while the server's covers it. */
		onoptions: (channel: Snowflake, rule: Rule | null) => void;
		/** Sets or clears the profile of a channel. Rejects when the server refuses. */
		onassign: (channel: Snowflake, profile: Uuid | null) => Promise<void>;
	}

	let { channels, rules, profiles, assignments, inherited, onwatch, onoptions, onassign }: Props =
		$props();

	/** One row of the table: a category heading, a channel, a thread, or an unseen channel. */
	interface Row {
		id: Snowflake;
		name: string;
		channel: GuildChannel | null;
		rule: Rule | null;
		kind: 'category' | 'channel' | 'thread' | 'unseen';
	}

	let filter = $state('');
	const pending = new SvelteSet<Snowflake>();

	const serverOn = $derived(rules.some((rule) => rule.channel_id === null && rule.enabled));
	const ruleOf = $derived(
		new Map(
			rules
				.filter((rule) => rule.channel_id !== null)
				.map((rule) => [rule.channel_id as Snowflake, rule])
		)
	);
	const byId = $derived(new Map(channels.map((channel) => [channel.id, channel])));
	const profileOf = $derived(
		new Map(
			assignments
				.filter((a) => a.scope.kind === 'channel')
				.map((a) => [a.scope.kind === 'channel' ? a.scope.channel_id : '', a.profile_id])
		)
	);
	const needle = $derived(filter.trim().toLowerCase());

	/** Whether a channel is watched: by its own rule, or by the server's when it has none. */
	const isOn = (rule: Rule | null) => (rule ? rule.enabled : serverOn);

	/** Whether a node, or anything under it, matches the filter. */
	function matches(node: ChannelNode): boolean {
		if (!needle) return true;
		if (node.kind !== 'category' && node.name.toLowerCase().includes(needle)) return true;
		return (node.children ?? []).some(matches);
	}

	const rows = $derived.by((): Row[] => {
		const out: Row[] = [];
		const leaf = (node: ChannelNode, kind: 'channel' | 'thread') => {
			out.push({
				id: node.id,
				name: node.name,
				channel: node.channel,
				rule: ruleOf.get(node.id) ?? null,
				kind
			});
			for (const thread of (node.children ?? []).filter(matches)) leaf(thread, 'thread');
		};
		for (const node of channelTree(channels).filter(matches)) {
			if (node.kind === 'category') {
				out.push({ id: node.id, name: node.name, channel: null, rule: null, kind: 'category' });
				for (const child of (node.children ?? []).filter(matches)) leaf(child, 'channel');
			} else {
				leaf(node, 'channel');
			}
		}
		return out;
	});

	/** Channels the bot no longer sees that still carry a rule or a profile, so they stay reachable. */
	const unseen = $derived.by((): Row[] => {
		if (needle) return [];
		const ids = new Set<Snowflake>([...ruleOf.keys(), ...profileOf.keys()]);
		return [...ids]
			.filter((id) => !byId.has(id))
			.sort()
			.map((id) => ({
				id,
				name: id,
				channel: null,
				rule: ruleOf.get(id) ?? null,
				kind: 'unseen'
			}));
	});

	async function watch(row: Row, on: boolean) {
		pending.add(row.id);
		try {
			await onwatch(row.id, on);
		} catch {
			// The switch shows the rule as it still is.
		} finally {
			pending.delete(row.id);
		}
	}

	async function assign(event: Event & { currentTarget: HTMLSelectElement }, channel: Snowflake) {
		const select = event.currentTarget;
		const before = profileOf.get(channel) ?? '';
		const next = select.value;
		pending.add(channel);
		try {
			await onassign(channel, next || null);
		} catch {
			select.value = before;
		} finally {
			pending.delete(channel);
		}
	}

	/** With one profile there is nothing to choose, so the column stays out. */
	const choosable = $derived(profiles.length > 1);
	const watched = $derived(
		channels.filter((c) => c.kind !== 'category' && isOn(ruleOf.get(c.id) ?? null)).length
	);
	const channelCount = $derived(channels.filter((c) => c.kind !== 'category').length);
</script>

{#snippet channelRow(row: Row)}
	{@const rule = row.rule}
	{@const on = isOn(rule)}
	{@const own = rule && rule.enabled ? rule : null}
	{@const summary = own ? ruleSummary(own, channels) : []}
	{@const busy = pending.has(row.id)}
	<tr class="hover:preset-tonal">
		<td class={row.kind === 'thread' ? 'pl-10' : 'pl-4'}>
			<span class="flex min-w-0 items-center gap-2">
				{#if row.channel}
					<ChannelKindIcon kind={row.channel.kind} class="size-4 shrink-0 text-surface-600-400" />
					<span class="truncate {on ? 'font-medium' : ''}">{row.name}</span>
				{:else}
					<span class="truncate font-mono text-xs">{row.name}</span>
				{/if}
			</span>
		</td>
		<td>
			<span class="flex flex-wrap items-center gap-x-3 gap-y-1">
				<Switch checked={on} disabled={busy} onCheckedChange={(d) => watch(row, d.checked)}>
					<Switch.Control><Switch.Thumb /></Switch.Control>
					<Switch.Label class="sr-only">Watch {row.name}</Switch.Label>
					<Switch.HiddenInput />
				</Switch>
				{#if busy}
					<Spinner />
				{:else if on}
					<button
						type="button"
						class="btn-icon btn-icon-sm hover:preset-tonal"
						onclick={() => onoptions(row.id, own)}
						aria-label="Options for {row.name}"
						title="Options"
					>
						<SlidersHorizontalIcon class="size-4" />
					</button>
					{#if summary.length > 0}
						<span class="text-xs text-surface-600-400">{summary.join(' · ')}</span>
					{/if}
				{/if}
			</span>
		</td>
		{#if choosable}
			<td>
				<select
					class="select"
					value={profileOf.get(row.id) ?? ''}
					onchange={(event) => assign(event, row.id)}
					disabled={busy}
					aria-label="Profile of {row.name}"
				>
					<option value="">Same as server ({inherited})</option>
					{#each profiles as profile (profile.id)}
						<option value={profile.id}>{profile.name}</option>
					{/each}
				</select>
			</td>
		{/if}
	</tr>
{/snippet}

{#snippet heading(text: string)}
	<tr>
		<th
			colspan={choosable ? 3 : 2}
			scope="rowgroup"
			class="pt-4 pb-1 text-left text-xs font-semibold tracking-wide text-surface-600-400 uppercase"
		>
			{text}
		</th>
	</tr>
{/snippet}

<div class="flex flex-col gap-3">
	<div class="flex flex-wrap items-center gap-3 px-4 pt-4">
		<SearchInput
			bind:value={filter}
			placeholder="Filter channels"
			debounce={0}
			class="min-w-0 flex-1"
		/>
		<span class="shrink-0 text-xs text-surface-600-400">
			{number(watched)} of {number(channelCount)} watched
		</span>
	</div>
	{#if channels.length === 0 && unseen.length === 0}
		<p class="px-4 py-8 text-center text-sm text-surface-600-400">The bot sees no channels here.</p>
	{:else if rows.length === 0 && unseen.length === 0}
		<p class="px-4 py-8 text-center text-sm text-surface-600-400">No channel matches.</p>
	{:else}
		<!-- Skeleton's table: every channel with its watch switch and its profile, categories as headings. -->
		<div class="max-h-[40rem] table-wrap overflow-y-auto px-2 pb-2">
			<table class="table">
				<thead class="sticky top-0 z-10 bg-surface-100-900">
					<tr>
						<th scope="col">Channel</th>
						<th scope="col">Watch</th>
						{#if choosable}<th scope="col" class="w-64">Profile</th>{/if}
					</tr>
				</thead>
				<tbody>
					{#each rows as row (row.id)}
						{#if row.kind === 'category'}
							{@render heading(row.name)}
						{:else}
							{@render channelRow(row)}
						{/if}
					{/each}
					{#if unseen.length > 0}
						{@render heading('Channels the bot no longer sees')}
						{#each unseen as row (row.id)}
							{@render channelRow(row)}
						{/each}
					{/if}
				</tbody>
			</table>
		</div>
	{/if}
</div>
