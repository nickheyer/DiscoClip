<script lang="ts">
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import PlayIcon from '@lucide/svelte/icons/play';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import SquareIcon from '@lucide/svelte/icons/square';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { applications, rules as rulesApi } from '$lib/api/endpoints';
	import type { ApplicationView, BotGuild, Rule, Snowflake } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import Identifier from '$lib/components/Identifier.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Count from '$lib/components/Count.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { stopWatching, watchServer } from '$lib/components/guild/watching';
	import { feed } from '$lib/events.svelte';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? '');

	let app = $state<ApplicationView | null>(null);
	let guilds = $state<BotGuild[]>([]);
	/** Every watch rule of this application. */
	let rules = $state<Rule[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	const bot = $derived(app ? (feed.bots[app.id] ?? app.bot) : null);
	const canBots = $derived(session.can('manage_bots'));
	const canRules = $derived(session.can('manage_watch_rules'));
	/** The rule watching each server whole, by server. */
	const serverRuleOf = $derived(
		new Map(rules.filter((rule) => rule.channel_id === null).map((rule) => [rule.guild_id, rule]))
	);
	const watchPending = new SvelteSet<Snowflake>();
	/** The bot is up or on its way up, so it can be stopped or restarted. */
	const running = $derived(
		bot !== null &&
			(bot.state === 'connected' || bot.state === 'starting' || bot.state === 'retrying')
	);

	let botPending = $state<'start' | 'stop' | 'restart' | null>(null);
	let registering = $state(false);
	let confirmDelete = $state(false);

	// Settings
	let name = $state('');
	let newToken = $state('');
	let saving = $state(false);
	// Discord login
	let newSecret = $state('');
	let login = $state(false);
	let savingLogin = $state(false);

	function syncForm(view: ApplicationView) {
		name = view.name;
		login = view.login;
		newToken = '';
		newSecret = '';
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [view, list, all] = await Promise.all([
				applications.get(id),
				applications.guilds(id),
				canRules ? rulesApi.list() : Promise.resolve([])
			]);
			if (current !== requestId) return;
			app = view;
			guilds = list;
			rules = all.filter((rule) => rule.application_id === id);
			syncForm(view);
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

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!app) return;
		saving = true;
		try {
			app = await applications.update(id, {
				name: name.trim() !== app.name ? name.trim() : undefined,
				bot_token: newToken.trim() || undefined
			});
			syncForm(app);
			notify.success('Settings saved');
		} catch (err) {
			reportError(err, 'Could not save the settings');
		} finally {
			saving = false;
		}
	}

	async function saveLogin(event: SubmitEvent) {
		event.preventDefault();
		if (!app) return;
		savingLogin = true;
		try {
			app = await applications.update(id, {
				client_secret: newSecret.trim() || undefined,
				login: login !== app.login ? login : undefined
			});
			syncForm(app);
			notify.success(app.login ? 'Discord login on' : 'Discord login off');
		} catch (err) {
			reportError(err, 'Could not save the Discord login');
		} finally {
			savingLogin = false;
		}
	}

	async function register() {
		if (!app) return;
		registering = true;
		try {
			const result = await applications.register(id);
			app = { ...app, commands: result };
			if (result.error) notify.error('Slash commands not registered', result.error);
			else notify.success('Slash commands registered');
		} catch (err) {
			reportError(err, 'Could not register the slash commands');
		} finally {
			registering = false;
		}
	}

	async function remove() {
		await applications.remove(id);
		notify.success('Application deleted');
		await goto(resolve('/applications'));
	}

	const guildName = (guildId: Snowflake) =>
		guilds.find((guild) => guild.guild_id === guildId)?.name ?? guildId;

	async function unwatch(rule: Rule) {
		const gone = await stopWatching(rule, rules);
		rules = rules.filter((r) => !gone.includes(r.id));
		notify.success('Stopped watching', guildName(rule.guild_id));
	}

	/** Watches every channel of a server, or stops. */
	async function toggleWatch(guild: BotGuild, on: boolean) {
		const rule = serverRuleOf.get(guild.guild_id) ?? null;
		watchPending.add(guild.guild_id);
		try {
			if (on) {
				const saved = await watchServer(id, guild.guild_id, rule);
				rules = rule ? rules.map((r) => (r.id === saved.id ? saved : r)) : [...rules, saved];
				notify.success('Watching every channel', guild.name);
			} else if (rule) {
				await unwatch(rule);
			}
		} catch (err) {
			reportError(err, on ? 'Could not watch the server' : 'Could not stop watching the server');
		} finally {
			watchPending.delete(guild.guild_id);
		}
	}

	const present = $derived(guilds.filter((guild) => guild.present).length);
	const installUrl = $derived(app?.install_url ?? '');

	const guildColumns = $derived.by((): Column<BotGuild>[] => [
		{ key: 'name', label: 'Server', cell: guildCell, sortable: true, value: (g) => g.name },
		...(canRules
			? [
					{
						key: 'watch',
						label: 'Watch',
						cell: watchCell,
						sortable: true,
						value: (g: BotGuild) => (serverRuleOf.get(g.guild_id)?.enabled ? 1 : 0)
					}
				]
			: []),
		{
			key: 'members',
			label: 'Members',
			align: 'right',
			sortable: true,
			value: (g) => g.member_count
		},
		{ key: 'joined', label: 'Joined', cell: joinedCell, sortable: true, value: (g) => g.joined_at }
	]);

	const back = { href: resolve('/applications'), label: 'Applications' };
</script>

{#snippet guildCell(guild: BotGuild)}
	<span class="flex items-center gap-3">
		<GuildIcon guild={guild.guild_id} hash={guild.icon} name={guild.name} size={28} />
		<span class="truncate font-medium">{guild.name}</span>
		{#if !guild.present}
			<Status present={false} title={guild.left_at ? `Left ${guild.left_at}` : undefined} />
		{/if}
	</span>
{/snippet}
{#snippet joinedCell(guild: BotGuild)}
	<Timestamp at={guild.joined_at} class="whitespace-nowrap" />
{/snippet}
{#snippet watchCell(guild: BotGuild)}
	{@const busy = watchPending.has(guild.guild_id)}
	<span class="flex items-center gap-2">
		<Switch
			checked={serverRuleOf.get(guild.guild_id)?.enabled ?? false}
			disabled={busy || !guild.present}
			onCheckedChange={(details) => toggleWatch(guild, details.checked)}
		>
			<Switch.Control><Switch.Thumb /></Switch.Control>
			<Switch.Label class="sr-only">Watch every channel in {guild.name}</Switch.Label>
			<Switch.HiddenInput />
		</Switch>
		{#if busy}<Spinner />{/if}
	</span>
{/snippet}

{#if error && !loading}
	<PageHeader title="Application" {back} />
	<ErrorState {error} title="This application could not be loaded" onretry={load} />
{:else if !app || !bot}
	<PageHeader title="Application" {back} />
	<div class="space-y-3" aria-busy="true">
		<div class="h-8 placeholder w-1/2 animate-pulse"></div>
		<div class="h-32 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader title={app.name} {back}>
		<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-surface-600-400">
			<Status bot={bot.state} />
			{#if bot.state === 'connected'}
				<span>as {bot.user}</span>
			{/if}
			<span>since <Timestamp at={bot.since} /></span>
			<span class="inline-flex items-center gap-1">
				Client id <Identifier value={app.client_id} full label="Copy client id" />
			</span>
		</div>
		{#snippet actions()}
			{#if canBots}
				{#if running}
					<button
						type="button"
						class="btn preset-tonal"
						onclick={() => botAction('restart')}
						disabled={botPending !== null}
					>
						{#if botPending === 'restart'}<Spinner />{:else}<RotateCcwIcon class="size-4" />{/if}
						Restart
					</button>
					<button
						type="button"
						class="btn preset-tonal"
						onclick={() => botAction('stop')}
						disabled={botPending !== null}
					>
						{#if botPending === 'stop'}<Spinner />{:else}<SquareIcon class="size-4" />{/if}
						Stop
					</button>
				{:else}
					<button
						type="button"
						class="btn preset-tonal"
						onclick={() => botAction('start')}
						disabled={botPending !== null}
					>
						{#if botPending === 'start'}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
						Start
					</button>
				{/if}
			{/if}
			<button type="button" class="btn preset-tonal-error" onclick={() => (confirmDelete = true)}>
				<Trash2Icon class="size-4" />
				Delete
			</button>
		{/snippet}
	</PageHeader>

	{#if bot.state === 'retrying' || bot.state === 'failed'}
		<div
			class="flex items-start gap-3 card p-4 text-sm {bot.state === 'failed'
				? 'preset-tonal-error'
				: 'preset-tonal-warning'}"
			role="alert"
		>
			<TriangleAlertIcon class="mt-0.5 size-4 shrink-0" />
			<div class="min-w-0 space-y-1">
				<p class="font-medium">
					{#if bot.state === 'retrying'}
						The bot lost its connection and is trying again. Attempt {number(bot.attempt)}, next
						<Timestamp at={bot.next_attempt_at} />.
					{:else}
						The bot stopped with an error.
					{/if}
				</p>
				<p class="break-words">{bot.error}</p>
			</div>
		</div>
	{/if}

	<div class="grid items-start gap-6 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
		<Card title="Servers" count={number(present)} flush>
			{#snippet actions()}
				<CopyButton text={installUrl} label="Copy invite link" withText />
				<a href={installUrl} class="btn preset-filled-primary-500" target="_blank" rel="noreferrer">
					<ExternalLinkIcon class="size-4" />
					Add to a server
				</a>
			{/snippet}
			{#if app.commands.error}
				<div class="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 pt-3 text-sm" role="alert">
					<span class="min-w-0 flex-1 text-error-600-400">
						Slash commands could not be registered. {app.commands.error}
					</span>
					<button
						type="button"
						class="btn preset-tonal btn-sm"
						onclick={register}
						disabled={registering}
					>
						{#if registering}<Spinner />{/if}
						Try again
					</button>
				</div>
			{:else if app.commands.mode === 'off'}
				<p class="px-4 pt-3 text-sm text-surface-600-400">Slash commands are turned off.</p>
			{:else if app.commands.registered_at}
				<p class="px-4 pt-3 text-sm text-surface-600-400">
					Slash commands registered
					{#if app.commands.mode === 'guilds'}
						in <Count value={app.commands.guilds.length} noun="server" />
					{/if}
					<Timestamp at={app.commands.registered_at} />.
				</p>
			{:else}
				<div class="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 pt-3 text-sm">
					<span class="min-w-0 flex-1 text-surface-600-400">
						Slash commands are not registered yet.
					</span>
					<button
						type="button"
						class="btn preset-tonal btn-sm"
						onclick={register}
						disabled={registering}
					>
						{#if registering}<Spinner />{/if}
						Register
					</button>
				</div>
			{/if}
			<DataTable
				rows={guilds}
				columns={guildColumns}
				rowKey={(g) => g.guild_id}
				rowHref={(g) =>
					resolve('/(app)/applications/[id]/guilds/[guild]', { id, guild: g.guild_id })}
				rowLabel="Open"
				flush
				class="p-2"
			>
				{#snippet empty()}
					The bot is not in a server yet. Add it to one to start.
				{/snippet}
			</DataTable>
		</Card>

		<div class="flex min-w-0 flex-col gap-6">
			<Card title="Settings">
				<form class="space-y-4" onsubmit={save}>
					<Field label="Name" for="app-name" required>
						<input id="app-name" class="input" type="text" bind:value={name} required />
					</Field>
					<Field label="Bot token" for="app-token">
						<input
							id="app-token"
							class="input font-mono"
							type="password"
							bind:value={newToken}
							autocomplete="off"
							placeholder="********"
						/>
					</Field>
					<div class="flex justify-end">
						<button type="submit" class="btn preset-filled" disabled={saving}>
							{#if saving}<Spinner />{/if}
							Save
						</button>
					</div>
				</form>
			</Card>

			<!-- Discord login: the switch, the client secret it needs, and the redirect the
			     Developer Portal must know, kept apart from the bot's own settings. -->
			<Card title="Discord login">
				{#snippet actions()}
					<Status enabled={app?.login ?? false} />
				{/snippet}
				<form class="space-y-4" onsubmit={saveLogin}>
					<Switch checked={login} onCheckedChange={(details) => (login = details.checked)}>
						<Switch.Control><Switch.Thumb /></Switch.Control>
						<Switch.Label>Let people sign in with Discord</Switch.Label>
						<Switch.HiddenInput />
					</Switch>
					<Field
						label="Client secret"
						for="app-secret"
						required={login && !app.has_client_secret}
						help="From the OAuth2 page of the application in the Developer Portal."
					>
						<input
							id="app-secret"
							class="input font-mono"
							type="password"
							bind:value={newSecret}
							autocomplete="off"
							required={login && !app.has_client_secret}
							placeholder={app.has_client_secret ? '********' : ''}
						/>
					</Field>
					<div class="label min-w-0">
						<span class="label-text">Redirect URL</span>
						<p class="flex items-center gap-1">
							<span class="font-mono text-xs break-all">{app.login_callback_url}</span>
							<CopyButton text={app.login_callback_url} label="Copy redirect URL" />
						</p>
						<p class="text-xs text-surface-600-400">Add it under Redirects on that same page.</p>
					</div>
					<div class="flex justify-end">
						<button type="submit" class="btn preset-filled" disabled={savingLogin}>
							{#if savingLogin}<Spinner />{/if}
							Save
						</button>
					</div>
				</form>
			</Card>
		</div>
	</div>
{/if}

<Confirm
	bind:open={confirmDelete}
	title="Delete {app?.name ?? 'this application'}?"
	message="The bot stops. Its watch rules and server records go with it."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
