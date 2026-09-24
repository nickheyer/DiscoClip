<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { applications, rules as rulesApi } from '$lib/api/endpoints';
	import type { ApplicationView, Rule } from '$lib/api/types';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { feed } from '$lib/events.svelte';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	let apps = $state<ApplicationView[]>([]);
	let rules = $state<Rule[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let tab = $state<'applications' | 'rules'>('applications');

	let addOpen = $state(false);
	let token = $state('');
	let name = $state('');
	let clientSecret = $state('');
	let adding = $state(false);

	async function load() {
		loading = true;
		error = null;
		try {
			const [list, all] = await Promise.all([
				applications.list(),
				session.can('manage_watch_rules') ? rulesApi.list() : Promise.resolve([])
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

	const COMMAND_MODE = {
		off: 'Commands off',
		global: 'Global commands',
		guilds: 'Commands in chosen servers'
	};

	const ruleColumns: Column<Rule>[] = [
		{ key: 'application', label: 'Application', value: (rule) => appName(rule.application_id) },
		{ key: 'guild', label: 'Server', value: (rule) => rule.guild_id, class: 'font-mono text-xs' },
		{
			key: 'channel',
			label: 'Channel',
			value: (rule) => rule.channel_id,
			class: 'font-mono text-xs'
		},
		{
			key: 'post_to',
			label: 'Posts to',
			value: (rule) => rule.post_to ?? 'Same channel',
			class: 'font-mono text-xs'
		},
		{
			key: 'allow',
			label: 'Allowed',
			value: (rule) =>
				`${number(rule.allow_users.length)} members · ${number(rule.allow_roles.length)} roles`
		},
		{ key: 'enabled', label: 'Enabled', cell: enabledCell },
		{ key: 'updated', label: 'Updated', cell: updatedCell }
	];
</script>

{#snippet enabledCell(rule: Rule)}
	<Status enabled={rule.enabled} />
{/snippet}
{#snippet updatedCell(rule: Rule)}
	<RelativeTime at={rule.updated_at} class="whitespace-nowrap" />
{/snippet}

<PageHeader title="Applications" />

<Toolbar description="Each Discord application runs its own bot.">
	<button type="button" class="btn preset-filled-primary-500" onclick={() => (addOpen = true)}>
		<PlusIcon class="size-4" />
		Add application
	</button>
</Toolbar>

<div
	class="flex gap-1 border-b border-surface-200-800"
	role="tablist"
	aria-label="Applications and rules"
>
	<button
		type="button"
		role="tab"
		aria-selected={tab === 'applications'}
		class="border-b-2 px-3 py-2 text-sm font-medium {tab === 'applications'
			? 'border-primary-500'
			: 'border-transparent text-surface-600-400 hover:text-surface-950-50'}"
		onclick={() => (tab = 'applications')}
	>
		Applications
	</button>
	{#if session.can('manage_watch_rules')}
		<button
			type="button"
			role="tab"
			aria-selected={tab === 'rules'}
			class="border-b-2 px-3 py-2 text-sm font-medium {tab === 'rules'
				? 'border-primary-500'
				: 'border-transparent text-surface-600-400 hover:text-surface-950-50'}"
			onclick={() => (tab = 'rules')}
		>
			Watch rules
			<span class="ml-2 text-surface-600-400">{number(rules.length)}</span>
		</button>
	{/if}
</div>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if tab === 'applications'}
	{#if loading && apps.length === 0}
		<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3" aria-busy="true">
			{#each { length: 3 }, i (i)}
				<div class="h-36 placeholder animate-pulse"></div>
			{/each}
		</div>
	{:else if apps.length === 0}
		<EmptyState
			title="No applications yet"
			description="Add a bot token from the Discord Developer Portal to run a bot."
		>
			<button type="button" class="btn preset-filled-primary-500" onclick={() => (addOpen = true)}>
				<PlusIcon class="size-4" />
				Add application
			</button>
		</EmptyState>
	{:else}
		<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3" role="tabpanel">
			{#each apps as app (app.id)}
				{@const bot = botOf(app)}
				<a
					href={resolve('/(app)/applications/[id]', { id: app.id })}
					class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 transition hover:bg-surface-200-800 sm:p-6"
				>
					<div class="flex items-start justify-between gap-3">
						<div class="min-w-0">
							<p class="truncate font-semibold">{app.name}</p>
							<p class="font-mono text-xs text-surface-600-400">{app.client_id}</p>
						</div>
						<Status bot={bot.state} />
					</div>
					<dl class="grid grid-cols-2 gap-x-3 gap-y-1 text-sm">
						<dt class="text-surface-600-400">Bot</dt>
						<dd>
							{#if bot.state === 'connected'}
								{bot.user}
							{:else if bot.state === 'retrying'}
								Attempt {bot.attempt}
							{:else if bot.state === 'failed'}
								<span class="text-error-700-300">{bot.error}</span>
							{:else}
								Since <RelativeTime at={bot.since} />
							{/if}
						</dd>
						<dt class="text-surface-600-400">Commands</dt>
						<dd>{COMMAND_MODE[app.commands.mode]}</dd>
						<dt class="text-surface-600-400">Login</dt>
						<dd>{app.login ? 'On' : 'Off'}</dd>
						<dt class="text-surface-600-400">Added</dt>
						<dd><RelativeTime at={app.created_at} /></dd>
					</dl>
				</a>
			{/each}
		</div>
	{/if}
{:else}
	<div role="tabpanel">
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
	</div>
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
