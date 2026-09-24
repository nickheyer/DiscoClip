<script lang="ts">
	import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { guilds as guildsApi } from '$lib/api/endpoints';
	import type { Guild, GuildApplication, Snowflake } from '$lib/api/types';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import Status from '$lib/components/Status.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { reportError } from '$lib/toast.svelte';

	let guilds = $state<Guild[]>([]);
	let apps = $state<Record<Snowflake, GuildApplication[]>>({});
	let loading = $state(true);
	let refreshing = $state(false);
	let error = $state<unknown>(null);

	async function loadApplications(list: Guild[]) {
		const manageable = list.filter((guild) => guild.manageable);
		const entries = await Promise.all(
			manageable.map(async (guild) => {
				try {
					return [guild.id, await guildsApi.applications(guild.id)] as const;
				} catch (err) {
					reportError(err, `Could not list the bots in ${guild.name}`);
					return [guild.id, []] as const;
				}
			})
		);
		apps = Object.fromEntries(entries);
	}

	async function load() {
		loading = true;
		error = null;
		try {
			guilds = await guildsApi.list();
			await loadApplications(guilds);
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	async function refresh() {
		refreshing = true;
		try {
			guilds = await guildsApi.refresh();
			await loadApplications(guilds);
		} catch (err) {
			reportError(err, 'Could not refresh your servers');
		} finally {
			refreshing = false;
		}
	}

	onMount(() => void load());

	const manageable = $derived(guilds.filter((guild) => guild.manageable));
	const others = $derived(guilds.filter((guild) => !guild.manageable));
	const fetchedAt = $derived(guilds[0]?.fetched_at ?? null);
</script>

<PageHeader title="Servers" />

<Toolbar
	description="The Discord servers your linked account belongs to. Servers you manage on Discord open their watch rules here."
>
	{#if fetchedAt}
		<span class="mr-auto text-sm text-surface-600-400"
			>Fetched <RelativeTime at={fetchedAt} />.</span
		>
	{/if}
	<button type="button" class="btn preset-tonal" onclick={refresh} disabled={refreshing}>
		{#if refreshing}<Spinner />{:else}<RefreshCwIcon class="size-4" />{/if}
		Refresh from Discord
	</button>
</Toolbar>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if loading}
	<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3" aria-busy="true">
		{#each { length: 3 }, i (i)}
			<div class="h-28 placeholder animate-pulse"></div>
		{/each}
	</div>
{:else if guilds.length === 0}
	<EmptyState
		title="No servers yet"
		description="Link your Discord account under Account, then refresh."
	>
		<a href={resolve('/account')} class="btn preset-filled-primary-500">Open Account</a>
	</EmptyState>
{:else}
	<section class="space-y-3" aria-label="Servers you manage">
		<h2 class="h6">You manage ({manageable.length})</h2>
		{#if manageable.length === 0}
			<p class="text-sm text-surface-600-400">
				You do not manage any of these servers on Discord. Managing a server means owning it or
				holding Administrator or Manage Server.
			</p>
		{:else}
			<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
				{#each manageable as guild (guild.id)}
					{@const bots = apps[guild.id] ?? []}
					<div class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6">
						<div class="flex items-center gap-3">
							<GuildIcon guild={guild.id} hash={guild.icon} name={guild.name} size={40} />
							<div class="min-w-0">
								<p class="truncate font-semibold">{guild.name}</p>
								<p class="font-mono text-xs text-surface-600-400">{guild.id}</p>
							</div>
							{#if guild.owner}<span class="ml-auto text-sm text-surface-600-400">Owner</span>{/if}
						</div>
						{#if bots.length === 0}
							<p class="text-sm text-surface-600-400">No DiscoClip bot has joined this server.</p>
						{:else}
							<ul class="space-y-1">
								{#each bots as bot (bot.application_id)}
									<li>
										<a
											href={resolve('/(app)/applications/[id]/guilds/[guild]', {
												id: bot.application_id,
												guild: guild.id
											})}
											class="flex items-center justify-between rounded-base px-2 py-1.5 text-sm hover:preset-tonal"
										>
											<span class="truncate">{bot.name}</span>
											<Status present={bot.present} />
										</a>
									</li>
								{/each}
							</ul>
						{/if}
					</div>
				{/each}
			</div>
		{/if}
	</section>

	{#if others.length > 0}
		<section class="space-y-3" aria-label="Other servers">
			<h2 class="h6">Member of ({others.length})</h2>
			<ul class="flex flex-wrap gap-2">
				{#each others as guild (guild.id)}
					<li
						class="flex items-center gap-2 rounded-full bg-surface-100-900 py-1 pr-3 pl-1 text-sm"
					>
						<GuildIcon guild={guild.id} hash={guild.icon} name={guild.name} size={24} />
						{guild.name}
					</li>
				{/each}
			</ul>
		</section>
	{/if}
{/if}
