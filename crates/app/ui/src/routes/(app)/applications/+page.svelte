<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { Tabs } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { applications, rules as rulesApi } from '$lib/api/endpoints';
	import type { ApplicationView, RuleView } from '$lib/api/types';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { feed } from '$lib/events.svelte';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	type Tab = 'applications' | 'rules';

	let apps = $state<ApplicationView[]>([]);
	let rules = $state<RuleView[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let tab = $state<Tab>('applications');

	let addOpen = $state(false);
	let token = $state('');
	let name = $state('');
	let clientSecret = $state('');
	let adding = $state(false);

	const canRules = $derived(session.can('manage_watch_rules'));

	async function load() {
		loading = true;
		error = null;
		try {
			const [list, all] = await Promise.all([
				applications.list(),
				canRules ? rulesApi.list() : Promise.resolve([])
			]);
			apps = list;
			rules = all;
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	function pickTab(value: string) {
		if (value === 'applications' || value === 'rules') tab = value;
	}

	function botOf(app: ApplicationView) {
		return feed.bots[app.id] ?? app.bot;
	}

	async function add(event: SubmitEvent) {
		event.preventDefault();
		adding = true;
		try {
			const created = await applications.create({
				bot_token: token.trim(),
				name: name.trim() || undefined,
				client_secret: clientSecret.trim() || undefined
			});
			notify.success('Application added', created.name);
			addOpen = false;
			token = '';
			name = '';
			clientSecret = '';
			await load();
		} catch (err) {
			reportError(err, 'Could not add the application');
		} finally {
			adding = false;
		}
	}

	const appName = (id: string) => apps.find((app) => app.id === id)?.name ?? id;

	const ruleColumns: Column<RuleView>[] = [
		{
			key: 'guild',
			label: 'Server',
			cell: guildCell,
			sortable: true,
			value: (rule) => rule.guild_name ?? rule.guild_id
		},
		{ key: 'channel', label: 'Channel', cell: channelCell },
		{
			key: 'application',
			label: 'Application',
			sortable: true,
			value: (rule) => appName(rule.application_id)
		},
		{ key: 'enabled', label: 'Enabled', cell: enabledCell },
		{ key: 'updated', label: 'Updated', cell: updatedCell }
	];
</script>

<!-- The server by its name and icon as its bot recorded them; the id stands in for a server the bot never saw. -->
{#snippet guildCell(rule: RuleView)}
	<span class="flex items-center gap-3">
		<GuildIcon
			guild={rule.guild_id}
			hash={rule.guild_icon}
			name={rule.guild_name ?? rule.guild_id}
			size={28}
		/>
		{#if rule.guild_name}
			<span class="truncate font-medium">{rule.guild_name}</span>
		{:else}
			<span class="truncate font-mono text-xs">{rule.guild_id}</span>
		{/if}
	</span>
{/snippet}
<!-- A channel by its name while the bot sees it, else by its id. -->
{#snippet channelName(id: string, name: string | null)}
	{#if name}
		<span>#{name}</span>
	{:else}
		<span class="font-mono text-xs">{id}</span>
	{/if}
{/snippet}
{#snippet channelCell(rule: RuleView)}
	{#if rule.channel_id}
		{@render channelName(rule.channel_id, rule.channel_name)}
	{:else}
		Every channel
	{/if}
{/snippet}
{#snippet enabledCell(rule: RuleView)}
	<Status enabled={rule.enabled} />
{/snippet}
{#snippet updatedCell(rule: RuleView)}
	<Timestamp at={rule.updated_at} class="whitespace-nowrap" />
{/snippet}

<PageHeader title="Applications" description="Each Discord application runs its own bot.">
	{#snippet actions()}
		<button type="button" class="btn preset-filled-primary-500" onclick={() => (addOpen = true)}>
			<PlusIcon class="size-4" />
			Add application
		</button>
	{/snippet}
</PageHeader>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<!-- Skeleton's Tabs: applications on one, every watch rule across them on the other. -->
	<Tabs value={tab} onValueChange={(details) => pickTab(details.value)}>
		<Tabs.List class="overflow-x-auto">
			<Tabs.Trigger value="applications">Applications</Tabs.Trigger>
			{#if canRules}
				<Tabs.Trigger value="rules">
					Watch rules
					<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
						{number(rules.length)}
					</span>
				</Tabs.Trigger>
			{/if}
			<Tabs.Indicator />
		</Tabs.List>

		<Tabs.Content value="applications">
			{#if loading && apps.length === 0}
				<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3" aria-busy="true">
					{#each { length: 3 }, i (i)}
						<div class="h-40 placeholder animate-pulse"></div>
					{/each}
				</div>
			{:else if apps.length === 0}
				<EmptyState
					title="No applications yet"
					description="Add a bot token from the Discord Developer Portal to run a bot."
				>
					<button
						type="button"
						class="btn preset-filled-primary-500"
						onclick={() => (addOpen = true)}
					>
						<PlusIcon class="size-4" />
						Add application
					</button>
				</EmptyState>
			{:else}
				<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
					{#each apps as app (app.id)}
						{@const bot = botOf(app)}
						<a
							href={resolve('/(app)/applications/[id]', { id: app.id })}
							class="min-w-0 space-y-4 card preset-filled-surface-100-900 p-4"
						>
							<div class="flex items-start justify-between gap-3">
								<div class="min-w-0">
									<p class="truncate font-semibold">{app.name}</p>
									<p class="font-mono text-xs text-surface-600-400">{app.client_id}</p>
								</div>
								<Status bot={bot.state} class="shrink-0" />
							</div>
							<KeyValue>
								<KeyValueRow label="Bot">
									{#if bot.state === 'connected'}
										{bot.user}
									{:else if bot.state === 'retrying'}
										Attempt {number(bot.attempt)}
									{:else if bot.state === 'failed'}
										<span class="text-error-600-400">{bot.error}</span>
									{:else}
										Since <Timestamp at={bot.since} />
									{/if}
								</KeyValueRow>
								<KeyValueRow label="Login" value={app.login ? 'On' : 'Off'} />
								<KeyValueRow label="Added"><Timestamp at={app.created_at} /></KeyValueRow>
							</KeyValue>
						</a>
					{/each}
				</div>
			{/if}
		</Tabs.Content>

		{#if canRules}
			<Tabs.Content value="rules">
				<DataTable
					rows={rules}
					columns={ruleColumns}
					rowKey={(rule) => rule.id}
					{loading}
					rowHref={(rule) =>
						resolve('/(app)/applications/[id]/guilds/[guild]', {
							id: rule.application_id,
							guild: rule.guild_id
						})}
				>
					{#snippet empty()}
						No watch rules yet. Open a server on an application to add one.
					{/snippet}
				</DataTable>
			</Tabs.Content>
		{/if}
	</Tabs>
{/if}

<Modal
	bind:open={addOpen}
	title="Add a Discord application"
	description="From the Bot page of the application in the Discord Developer Portal."
	busy={adding}
>
	<form id="add-application" class="space-y-4" onsubmit={add}>
		<Field label="Bot token" for="app-token" required help="Kept encrypted. It is not shown again.">
			<input
				id="app-token"
				class="input font-mono"
				type="password"
				bind:value={token}
				required
				autocomplete="off"
			/>
		</Field>
		<Field label="Name" for="app-name" help="Defaults to the name Discord reports.">
			<input id="app-name" class="input" type="text" bind:value={name} />
		</Field>
		<Field label="Client secret" for="app-secret" help="Needed for Discord login. Optional.">
			<input
				id="app-secret"
				class="input font-mono"
				type="password"
				bind:value={clientSecret}
				autocomplete="off"
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (addOpen = false)}
			disabled={adding}>Cancel</button
		>
		<button
			type="submit"
			form="add-application"
			class="btn preset-filled-primary-500"
			disabled={adding}
		>
			{#if adding}<Spinner />{/if}
			Add and start the bot
		</button>
	{/snippet}
</Modal>
