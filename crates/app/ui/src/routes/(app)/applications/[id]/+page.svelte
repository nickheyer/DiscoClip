<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import PlayIcon from '@lucide/svelte/icons/play';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import SquareIcon from '@lucide/svelte/icons/square';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { applications } from '$lib/api/endpoints';
	import type {
		ApplicationView,
		BotGuild,
		CommandMode,
		CommandsView,
		Snowflake
	} from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import { feed } from '$lib/events.svelte';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? '');

	let app = $state<ApplicationView | null>(null);
	let commands = $state<CommandsView | null>(null);
	let guilds = $state<BotGuild[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	const bot = $derived(app ? (feed.bots[app.id] ?? app.bot) : null);
	const canBots = $derived(session.can('manage_bots'));

	let botPending = $state<'start' | 'stop' | 'restart' | null>(null);

	// Credentials
	let name = $state('');
	let newToken = $state('');
	let newSecret = $state('');
	let removeSecret = $state(false);
	let login = $state(false);
	let savingCredentials = $state(false);

	// Commands
	let mode = $state<CommandMode>('off');
	let commandGuilds = $state<Snowflake[]>([]);
	let savingCommands = $state(false);
	let registering = $state(false);

	// Install
	let installGuild = $state('');
	let installUrl = $state('');
	let installPending = $state(false);

	let confirmDelete = $state(false);

	function syncForms(view: ApplicationView, cmds: CommandsView) {
		name = view.name;
		login = view.login;
		mode = cmds.mode;
		commandGuilds = [...cmds.guilds];
		installUrl = view.install_url;
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [view, cmds, list] = await Promise.all([
				applications.get(id),
				applications.commands(id),
				applications.guilds(id)
			]);
			if (current !== requestId) return;
			app = view;
			commands = cmds;
			guilds = list;
			syncForms(view, cmds);
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void id;
		void load();
	});

	onMount(() => {
		const timer = setInterval(() => {
			if (app)
				applications
					.guilds(id)
					.then((list) => (guilds = list))
					.catch(() => undefined);
		}, 60_000);
		return () => clearInterval(timer);
	});

	async function botAction(action: 'start' | 'stop' | 'restart') {
		botPending = action;
		try {
			app = await applications[action](id);
			notify.success(
				{ start: 'Bot started', stop: 'Bot stopped', restart: 'Bot restarting' }[action]
			);
		} catch (err) {
			reportError(err, `Could not ${action} the bot`);
		} finally {
			botPending = null;
		}
	}

	async function saveCredentials(event: SubmitEvent) {
		event.preventDefault();
		if (!app) return;
		savingCredentials = true;
		try {
			app = await applications.update(id, {
				name: name.trim() !== app.name ? name.trim() : undefined,
				bot_token: newToken.trim() || undefined,
				client_secret: removeSecret ? null : newSecret.trim() || undefined,
				login: login !== app.login ? login : undefined
			});
			newToken = '';
			newSecret = '';
			removeSecret = false;
			notify.success('Application saved');
		} catch (err) {
			reportError(err, 'Could not save the application');
		} finally {
			savingCredentials = false;
		}
	}

	async function saveCommands(event: SubmitEvent) {
		event.preventDefault();
		savingCommands = true;
		try {
			commands = await applications.setCommands(id, {
				mode,
				guilds: mode === 'guilds' ? commandGuilds : []
			});
			notify.success(commands.error ? 'Scope saved, registration failed' : 'Commands registered');
			app = await applications.get(id);
		} catch (err) {
			reportError(err, 'Could not save the command scope');
		} finally {
			savingCommands = false;
		}
	}

	async function register() {
		registering = true;
		try {
			commands = await applications.register(id);
			if (commands.error) notify.error('Registration failed', commands.error);
			else notify.success('Commands registered');
		} catch (err) {
			reportError(err, 'Could not register the commands');
		} finally {
			registering = false;
		}
	}

	async function fetchInstall() {
		installPending = true;
		try {
			const link = await applications.install(id, installGuild.trim() || undefined);
			installUrl = link.url;
		} catch (err) {
			reportError(err, 'Could not build the install link');
		} finally {
			installPending = false;
		}
	}

	async function remove() {
		await applications.remove(id);
		notify.success('Application deleted');
		await goto(resolve('/applications'));
	}

	function toggleGuild(guild: Snowflake) {
		commandGuilds = commandGuilds.includes(guild)
			? commandGuilds.filter((g) => g !== guild)
			: [...commandGuilds, guild];
	}

	const guildColumns: Column<BotGuild>[] = [
		{ key: 'name', label: 'Server', cell: guildCell, sortable: true, value: (g) => g.name },
		{
			key: 'members',
			label: 'Members',
			align: 'right',
			sortable: true,
			value: (g) => g.member_count
		},
		{
			key: 'present',
			label: 'Bot',
			cell: presentCell,
			sortable: true,
			value: (g) => (g.present ? 1 : 0)
		},
		{ key: 'joined', label: 'Joined', cell: joinedCell, sortable: true, value: (g) => g.joined_at }
	];
</script>

{#snippet guildCell(guild: BotGuild)}
	<span class="flex items-center gap-2">
		<GuildIcon guild={guild.guild_id} hash={guild.icon} name={guild.name} size={28} />
		<span class="min-w-0">
			<span class="block truncate font-medium">{guild.name}</span>
			<span class="block font-mono text-xs text-surface-600-400">{guild.guild_id}</span>
		</span>
	</span>
{/snippet}
{#snippet presentCell(guild: BotGuild)}
	<span title={guild.present || !guild.left_at ? undefined : `Left ${guild.left_at}`}>
		<Status present={guild.present} />
	</span>
{/snippet}
{#snippet joinedCell(guild: BotGuild)}
	<RelativeTime at={guild.joined_at} class="whitespace-nowrap" />
{/snippet}

{#if error && !loading}
	<PageHeader title="Application" />
	<ErrorState {error} title="This application could not be loaded" onretry={load} />
{:else if !app || !commands || !bot}
	<PageHeader title="Application" />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-40 placeholder animate-pulse"></div>
	</div>
{:else}
	<a
		href={resolve('/applications')}
		class="link-body inline-flex items-center gap-1 text-sm text-surface-700-300 hover:text-surface-950-50"
	>
		<ArrowLeftIcon class="size-4" />
		All applications
	</a>

	<PageHeader title={app.name}>
		<p class="flex items-center gap-2 font-mono text-xs text-surface-600-400">
			{app.client_id}
			<CopyButton text={app.client_id} label="Copy client id" />
		</p>
	</PageHeader>

	<Toolbar>
		<button type="button" class="btn preset-tonal-error" onclick={() => (confirmDelete = true)}>
			<Trash2Icon class="size-4" />
			Delete
		</button>
	</Toolbar>

	<div class="grid gap-6 lg:grid-cols-2">
		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Bot"
		>
			<div class="flex items-center justify-between">
				<h2 class="h6">Bot</h2>
				<Status bot={bot.state} />
			</div>
			<KeyValue>
				<KeyValueRow label="Since"><RelativeTime at={bot.since} /></KeyValueRow>
				{#if bot.state === 'connected'}
					<KeyValueRow label="Logged in as" value={bot.user} />
				{:else if bot.state === 'retrying'}
					<KeyValueRow label="Error" value={bot.error} />
					<KeyValueRow label="Attempt" value={bot.attempt} />
					<KeyValueRow label="Next attempt"><RelativeTime at={bot.next_attempt_at} /></KeyValueRow>
				{:else if bot.state === 'failed'}
					<KeyValueRow label="Error" value={bot.error} />
				{:else if bot.state === 'disabled'}
					<KeyValueRow label="Token" value="None saved" />
				{/if}
				<KeyValueRow label="Enabled" value={app.enabled ? 'Yes' : 'No, stays stopped'} />
			</KeyValue>
			{#if canBots}
				<div class="flex flex-wrap gap-2">
					<button
						type="button"
						class="btn preset-filled btn-sm"
						onclick={() => botAction('start')}
						disabled={botPending !== null || bot.state === 'connected'}
					>
						{#if botPending === 'start'}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
						Start
					</button>
					<button
						type="button"
						class="btn preset-tonal btn-sm"
						onclick={() => botAction('stop')}
						disabled={botPending !== null || bot.state === 'stopped' || bot.state === 'disabled'}
					>
						{#if botPending === 'stop'}<Spinner />{:else}<SquareIcon class="size-4" />{/if}
						Stop
					</button>
					<button
						type="button"
						class="btn preset-tonal btn-sm"
						onclick={() => botAction('restart')}
						disabled={botPending !== null}
					>
						{#if botPending === 'restart'}<Spinner />{:else}<RotateCcwIcon class="size-4" />{/if}
						Restart
					</button>
				</div>
			{/if}
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Credentials"
		>
			<h2 class="h6">Credentials and login</h2>
			<form class="space-y-4" onsubmit={saveCredentials}>
				<Field label="Name" for="app-name" required>
					<input id="app-name" class="input" type="text" bind:value={name} required />
				</Field>
				<Field
					label="Replace bot token"
					for="app-token"
					help="Leave empty to keep the saved token."
				>
					<input
						id="app-token"
						class="input font-mono"
						type="password"
						bind:value={newToken}
						autocomplete="off"
					/>
				</Field>
				<Field
					label="Client secret"
					for="app-secret"
					help={app.has_client_secret
						? 'A secret is saved. Enter a new one to replace it.'
						: 'No secret saved. Discord login needs one.'}
				>
					<input
						id="app-secret"
						class="input font-mono"
						type="password"
						bind:value={newSecret}
						autocomplete="off"
						disabled={removeSecret}
					/>
				</Field>
				{#if app.has_client_secret}
					<label class="flex items-center gap-2 text-sm">
						<input class="checkbox" type="checkbox" bind:checked={removeSecret} />
						Remove the client secret
					</label>
				{/if}
				<label class="flex items-center gap-2 text-sm">
					<input
						class="checkbox"
						type="checkbox"
						bind:checked={login}
						disabled={!app.has_client_secret && !newSecret.trim()}
					/>
					Offer Discord login with this application
				</label>
				<div class="rounded-base bg-surface-200-800 p-3 text-sm">
					<p class="text-surface-600-400">
						Register this callback at Discord under OAuth2 redirects:
					</p>
					<p class="mt-1 flex items-center gap-1 font-mono break-all">
						{app.login_callback_url}
						<CopyButton text={app.login_callback_url} label="Copy callback URL" />
					</p>
				</div>
				<div class="flex justify-end">
					<button type="submit" class="btn preset-filled-primary-500" disabled={savingCredentials}>
						{#if savingCredentials}<Spinner />{/if}
						Save
					</button>
				</div>
			</form>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Slash commands"
		>
			<h2 class="h6">Slash commands</h2>
			<form class="space-y-4" onsubmit={saveCommands}>
				<fieldset class="space-y-2">
					<legend class="text-sm font-medium">Where /clip and /status are registered</legend>
					{#each [['off', 'Nowhere'], ['global', 'Every server the bot is in'], ['guilds', 'Chosen servers only']] as [value, label] (value)}
						<label class="flex items-center gap-2 text-sm">
							<input class="radio" type="radio" name="mode" {value} bind:group={mode} />
							{label}
						</label>
					{/each}
				</fieldset>
				{#if mode === 'guilds'}
					<fieldset class="space-y-1">
						<legend class="text-sm font-medium">Servers</legend>
						{#if guilds.length === 0}
							<p class="text-sm text-surface-600-400">The bot is not in any server yet.</p>
						{/if}
						{#each guilds as guild (guild.guild_id)}
							<label class="flex items-center gap-2 text-sm">
								<input
									class="checkbox"
									type="checkbox"
									checked={commandGuilds.includes(guild.guild_id)}
									onchange={() => toggleGuild(guild.guild_id)}
								/>
								<GuildIcon guild={guild.guild_id} hash={guild.icon} name={guild.name} size={20} />
								{guild.name}
								{#if !guild.present}<span class="text-surface-600-400">left</span>{/if}
							</label>
						{/each}
					</fieldset>
				{/if}
				<KeyValue>
					<KeyValueRow label="Registered">
						{#if commands.registered_at}<RelativeTime
								at={commands.registered_at}
							/>{:else}Never{/if}
					</KeyValueRow>
					{#if commands.error}
						<KeyValueRow label="Last error"
							><span class="text-error-700-300">{commands.error}</span></KeyValueRow
						>
					{/if}
					<KeyValueRow
						label="Commands"
						value={commands.commands.map((c) => `/${c.name}`).join(', ')}
					/>
				</KeyValue>
				<div class="flex flex-wrap justify-end gap-2">
					<button
						type="button"
						class="btn preset-tonal"
						onclick={register}
						disabled={registering || savingCommands}
					>
						{#if registering}<Spinner />{/if}
						Register again
					</button>
					<button
						type="submit"
						class="btn preset-filled-primary-500"
						disabled={savingCommands || registering}
					>
						{#if savingCommands}<Spinner />{/if}
						Save and register
					</button>
				</div>
			</form>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Install"
		>
			<h2 class="h6">Add to a server</h2>
			<p class="text-sm text-surface-600-400">
				Open the link as a server admin to invite the bot with the permissions it needs.
			</p>
			<div class="flex items-end gap-2">
				<Field
					label="Server id"
					for="install-guild"
					help="Optional. Preselects the server."
					class="flex-1"
				>
					<input
						id="install-guild"
						class="input font-mono"
						type="text"
						bind:value={installGuild}
						placeholder="Any"
					/>
				</Field>
				<button
					type="button"
					class="btn preset-tonal"
					onclick={fetchInstall}
					disabled={installPending}
				>
					{#if installPending}<Spinner />{/if}
					Build link
				</button>
			</div>
			<div class="flex flex-wrap items-center gap-2">
				<a href={installUrl} class="btn preset-filled" target="_blank" rel="noreferrer">
					<ExternalLinkIcon class="size-4" />
					Open install link
				</a>
				<CopyButton text={installUrl} label="Copy install link" withText />
			</div>
		</section>
	</div>

	<section class="space-y-3" aria-label="Servers">
		<h2 class="h6">Servers ({number(guilds.length)})</h2>
		<DataTable
			rows={guilds}
			columns={guildColumns}
			rowKey={(g) => g.guild_id}
			rowHref={(g) => resolve('/(app)/applications/[id]/guilds/[guild]', { id, guild: g.guild_id })}
		>
			{#snippet empty()}
				The bot has not joined a server yet. Use the install link above.
			{/snippet}
		</DataTable>
	</section>
{/if}

<Confirm
	bind:open={confirmDelete}
	title="Delete {app?.name ?? 'this application'}?"
	message="The bot stops. Its watch rules and server records go with it."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
