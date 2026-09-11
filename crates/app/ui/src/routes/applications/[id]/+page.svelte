<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { applications, messageOf } from '$lib/api';
	import type { CommandMode, InstallLink } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import BotControls from '$lib/components/BotControls.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import TagInput from '$lib/components/TagInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatNumber, isSnowflake, secondsUntil } from '$lib/format';
	import { bots } from '$lib/state/bots.svelte';
	import { clock } from '$lib/state/clock.svelte';
	import { confirm } from '$lib/state/confirm.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { APPLICATION_NAME_MAX } from '$lib/validation';

	let { data }: { data: PageData } = $props();

	const app = $derived(data.app);
	const status = $derived(bots.status(app.id) ?? app.bot);
	const presentGuilds = $derived(data.guilds.filter((g) => g.present));
	const guildSuggestions = $derived(presentGuilds.map((g) => ({ value: g.guild_id, label: g.name })));
	const refresh = () => invalidate(`app:application:${app.id}`);

	// Install link

	let installFor = $state('');
	let install = $state<InstallLink | null>(null);
	let loadingInstall = $state(false);
	$effect.pre(() => {
		install = data.install;
		installFor = '';
	});

	async function pickGuild(guild: string) {
		installFor = guild;
		loadingInstall = true;
		try {
			install = await applications.install(app.id, guild || undefined);
		} catch (cause) {
			toast.error(`Could not build the install link: ${messageOf(cause)}`);
		} finally {
			loadingInstall = false;
		}
	}

	// Slash commands

	let mode = $state<CommandMode>('off');
	let scopeGuilds = $state<string[]>([]);
	let savingScope = $state(false);
	let registering = $state(false);
	let scopeError = $state<string | null>(null);
	$effect.pre(() => {
		mode = data.commands.mode;
		scopeGuilds = [...data.commands.guilds];
	});
	const scopeDirty = $derived(
		mode !== data.commands.mode ||
			JSON.stringify([...scopeGuilds].sort()) !== JSON.stringify([...data.commands.guilds].sort())
	);
	const scopeReady = $derived(mode !== 'guilds' || scopeGuilds.length > 0);

	async function saveScope(event: SubmitEvent) {
		event.preventDefault();
		if (!scopeReady) return;
		savingScope = true;
		scopeError = null;
		try {
			const view = await applications.setCommands(app.id, {
				mode,
				guilds: mode === 'guilds' ? scopeGuilds : []
			});
			toast.ok(
				view.error
					? 'Scope saved, but registering failed.'
					: mode === 'off'
						? 'Slash commands removed.'
						: 'Slash commands registered.'
			);
			await refresh();
		} catch (cause) {
			scopeError = messageOf(cause);
		} finally {
			savingScope = false;
		}
	}

	async function register() {
		registering = true;
		scopeError = null;
		try {
			const view = await applications.register(app.id);
			toast.ok(view.error ? 'Registering failed.' : 'Slash commands registered again.');
			await refresh();
		} catch (cause) {
			scopeError = messageOf(cause);
		} finally {
			registering = false;
		}
	}

	// Settings

	let name = $state('');
	$effect.pre(() => {
		name = app.name;
	});
	let savingName = $state(false);
	const nameProblem = $derived(
		name.trim() === '' ? 'A name is needed.' : name.trim().length > APPLICATION_NAME_MAX ? `A name is up to ${APPLICATION_NAME_MAX} characters.` : null
	);

	async function saveName(event: SubmitEvent) {
		event.preventDefault();
		if (nameProblem) return;
		savingName = true;
		try {
			await applications.update(app.id, { name: name.trim() });
			toast.ok('Renamed.');
			await refresh();
			await invalidate('app:applications');
		} catch (cause) {
			toast.error(`Could not rename: ${messageOf(cause)}`);
		} finally {
			savingName = false;
		}
	}

	let savingLogin = $state(false);

	async function setLogin(on: boolean) {
		savingLogin = true;
		try {
			await applications.update(app.id, { login: on });
			toast.ok(on ? 'Discord login is now offered through this application.' : 'Discord login withdrawn.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not change login: ${messageOf(cause)}`);
		} finally {
			savingLogin = false;
		}
	}

	let newToken = $state('');
	let savingToken = $state(false);

	async function replaceToken(event: SubmitEvent) {
		event.preventDefault();
		if (newToken.trim() === '') return;
		savingToken = true;
		try {
			const view = await applications.update(app.id, { bot_token: newToken.trim() });
			bots.put(view.id, view.bot);
			newToken = '';
			toast.ok('Bot token replaced; the bot restarts with it.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not replace the token: ${messageOf(cause)}`);
		} finally {
			savingToken = false;
		}
	}

	let newSecret = $state('');
	let savingSecret = $state(false);
	let removingSecret = $state(false);

	async function setSecret(event: SubmitEvent) {
		event.preventDefault();
		if (newSecret.trim() === '') return;
		savingSecret = true;
		try {
			await applications.update(app.id, { client_secret: newSecret.trim() });
			newSecret = '';
			toast.ok('Client secret set.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not set the secret: ${messageOf(cause)}`);
		} finally {
			savingSecret = false;
		}
	}

	async function removeSecret() {
		const ok = await confirm.ask({
			title: 'Remove the client secret?',
			message: app.login
				? 'Discord login through this application stops working, since it needs the secret.'
				: 'It can be set again any time.',
			confirmLabel: 'Remove',
			danger: true
		});
		if (!ok) return;
		removingSecret = true;
		try {
			await applications.update(app.id, { client_secret: null });
			toast.ok('Client secret removed.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the secret: ${messageOf(cause)}`);
		} finally {
			removingSecret = false;
		}
	}

	// Delete

	let deleting = $state(false);

	async function remove() {
		const ok = await confirm.ask({
			title: `Remove ${app.name}?`,
			message: 'The bot is retired and every watch rule and guild record of this application goes with it. The application itself stays on Discord.',
			confirmLabel: 'Remove application',
			danger: true,
			typed: app.name
		});
		if (!ok) return;
		deleting = true;
		try {
			await applications.remove(app.id);
			bots.forget(app.id);
			toast.ok(`Removed ${app.name}.`);
			await invalidate('app:applications');
			await goto('/applications');
		} catch (cause) {
			toast.error(`Could not remove the application: ${messageOf(cause)}`);
			deleting = false;
		}
	}
</script>

<svelte:head>
	<title>{app.name} · Applications · DiscoClip</title>
</svelte:head>

<PageHeader title={app.name} crumbs={[{ label: 'Applications', href: '/applications' }, { label: app.name }]}>
	{#snippet meta()}
		<BotBadge state={status.state} />
		{#if status.state === 'connected'}<span class="faint small">as @{status.user}</span>{/if}
		<span class="row faint small"><code>{app.client_id}</code><CopyButton text={app.client_id} square label="Copy client id" /></span>
	{/snippet}
	{#snippet actions()}
		<BotControls application={app.id} botState={status.state} size="md" onchange={() => refresh()} />
		<Button href={app.install_url} newTab icon="external">Add to a guild</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-header">
			<h2>Bot</h2>
			<span class="faint small">{bots.state === 'live' ? 'Updating live' : 'Status stream reconnecting'}</span>
		</div>
		<div class="card-body stack">
			<dl class="kv">
				<dt>State</dt>
				<dd class="row"><BotBadge state={status.state} /><span class="faint">since <Time value={status.since} /></span></dd>
				{#if status.state === 'connected'}
					<dt>Logged in as</dt>
					<dd>@{status.user}</dd>
				{/if}
				{#if status.state === 'retrying'}
					<dt>Attempt</dt>
					<dd>{formatNumber(status.attempt)} · next try in {secondsUntil(status.next_attempt_at, clock.now)}s</dd>
				{/if}
				<dt>Meant to run</dt>
				<dd>{app.enabled ? 'Yes' : 'No, it was stopped from the app and stays stopped until started'}</dd>
				<dt>Added</dt>
				<dd><Time value={app.created_at} mode="absolute" /></dd>
			</dl>
			{#if status.state === 'retrying'}
				<Alert tone="warn" title="A transient failure; the bot restarts by itself" message={status.error} />
			{:else if status.state === 'failed'}
				<Alert tone="danger" title="Discord rejected the token or the intents; start the bot again once fixed" message={status.error} />
			{:else if status.state === 'disabled'}
				<Alert tone="warn" message="No bot token is configured, so there is nothing to run." />
			{/if}
		</div>
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Install link</h2>
				<p class="hint">Adds the bot to a guild with the scopes and permissions it needs.</p>
			</div>
		</div>
		<div class="card-body stack">
			{#if install}
				<div class="install">
					<input class="input mono" readonly value={install.url} aria-label="Install URL" onfocus={(e) => (e.currentTarget as HTMLInputElement).select()} />
					<CopyButton text={install.url} variant="secondary" size="md" />
					<Button href={install.url} newTab icon="external" variant="primary">Open</Button>
				</div>
				<div class="grid-2">
					<Field label="Preselect a guild" for="install-guild" optional hint="Discord opens with this guild chosen, for anyone who can manage it.">
						<select id="install-guild" class="select" value={installFor} disabled={loadingInstall} onchange={(e) => pickGuild((e.currentTarget as HTMLSelectElement).value)}>
							<option value="">Let the person choose</option>
							{#each data.guilds as guild (guild.guild_id)}
								<option value={guild.guild_id}>{guild.name}{guild.present ? '' : ' (bot removed)'}</option>
							{/each}
						</select>
					</Field>
					<dl class="kv">
						<dt>Scopes</dt>
						<dd><div class="chips">{#each install.scopes as scope (scope)}<span class="chip">{scope}</span>{/each}</div></dd>
						<dt>Permissions</dt>
						<dd><div class="chips">{#each install.permissions as permission (permission)}<span class="chip">{permission}</span>{/each}</div></dd>
					</dl>
				</div>
			{/if}
		</div>
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Guilds</h2>
				<p class="hint">Where the bot is, and where it was removed from.</p>
			</div>
		</div>
		{#if data.guilds.length === 0}
			<div class="card-body"><Empty compact icon="server" title="Not in any guild yet" description="Use the install link above to add the bot to a guild." /></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr><th>Guild</th><th>Members</th><th>Status</th><th>Joined</th><th></th></tr>
					</thead>
					<tbody>
						{#each data.guilds as guild (guild.guild_id)}
							<tr>
								<td>
									<div class="row">
										<GuildIcon id={guild.guild_id} icon={guild.icon} name={guild.name} size={28} />
										<div class="stack-sm" style="gap:0">
											<span class="strong">{guild.name}</span>
											<code class="small">{guild.guild_id}</code>
										</div>
									</div>
								</td>
								<td class="num">{guild.member_count == null ? '—' : formatNumber(guild.member_count)}</td>
								<td>
									{#if guild.present}
										<Badge tone="ok" size="sm" dot>Present</Badge>
									{:else}
										<Badge tone="neutral" size="sm">Removed <Time value={guild.left_at} /></Badge>
									{/if}
								</td>
								<td><Time value={guild.joined_at} /></td>
								<td class="actions">
									<Button size="sm" variant="ghost" href={`/applications/${app.id}/guilds/${guild.guild_id}`} iconRight="chevron-right">Rules</Button>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Slash commands</h2>
				<p class="hint">Where <code>/clip</code> and <code>/status</code> are registered.</p>
			</div>
			<div class="row">
				{#if data.commands.registered_at}
					<span class="faint small">Registered <Time value={data.commands.registered_at} /></span>
				{/if}
				<Button size="sm" icon="refresh" loading={registering} disabled={data.commands.mode === 'off'} onclick={register}>Register again</Button>
			</div>
		</div>
		<form class="card-body stack" onsubmit={saveScope}>
			{#if scopeError}
				<Alert tone="danger" message={scopeError} onclose={() => (scopeError = null)} />
			{/if}
			{#if data.commands.error}
				<Alert tone="danger" title="The last registration failed" message={data.commands.error} />
			{/if}
			<div class="modes">
				<label class="radio">
					<input type="radio" name="mode" value="off" bind:group={mode} />
					<span><span class="strong">Off</span><span class="hint">Nowhere; any earlier registration is removed.</span></span>
				</label>
				<label class="radio">
					<input type="radio" name="mode" value="global" bind:group={mode} />
					<span><span class="strong">Global</span><span class="hint">Everywhere the bot is. Discord takes up to an hour to show them.</span></span>
				</label>
				<label class="radio">
					<input type="radio" name="mode" value="guilds" bind:group={mode} />
					<span><span class="strong">Chosen guilds</span><span class="hint">In the listed guilds only, at once.</span></span>
				</label>
			</div>
			{#if mode === 'guilds'}
				<Field label="Guilds" for="scope-guilds" hint="Guild ids. The ones the bot is in are suggested." error={scopeGuilds.length === 0 ? 'Pick at least one guild.' : null}>
					<TagInput id="scope-guilds" bind:values={scopeGuilds} placeholder="Guild id" validate={(v) => (isSnowflake(v) ? null : `${v} is not a Discord id`)} suggestions={guildSuggestions} />
				</Field>
			{/if}
			<div class="row-between">
				<div class="commands">
					{#each data.commands.commands as command (command.name)}
						<span class="command"><code>/{command.name}</code><span class="faint small">{command.description}</span></span>
					{/each}
				</div>
				<Button type="submit" variant="primary" loading={savingScope} disabled={!scopeDirty || !scopeReady}>Save and register</Button>
			</div>
		</form>
	</section>

	<section class="card">
		<div class="card-header"><h2>Settings</h2></div>
		<div class="card-body stack-lg">
			<form class="setting" onsubmit={saveName}>
				<Field label="Name" for="app-rename" error={nameProblem}>
					<input id="app-rename" class="input" bind:value={name} maxlength={APPLICATION_NAME_MAX} aria-invalid={nameProblem ? 'true' : undefined} />
				</Field>
				<Button type="submit" loading={savingName} disabled={!!nameProblem || name.trim() === app.name}>Rename</Button>
			</form>

			<div class="setting">
				<label class={['checkbox', !app.has_client_secret && 'disabled']}>
					<input type="checkbox" checked={app.login} disabled={!app.has_client_secret || savingLogin} onchange={(e) => setLogin((e.currentTarget as HTMLInputElement).checked)} />
					<span>
						<span class="strong">Offer Discord login through this application</span>
						<span class="hint">
							{#if app.has_client_secret}
								People log in to DiscoClip with Discord; the callback is <code>/api/auth/discord/callback</code> under the public URL. Only one application offers login at a time.
							{:else}
								Set the client secret first.
							{/if}
						</span>
					</span>
				</label>
			</div>

			<form class="setting" onsubmit={replaceToken}>
				<Field label="Replace bot token" for="app-token" hint="The bot restarts with the new token at once.">
					<PasswordInput id="app-token" bind:value={newToken} mono />
				</Field>
				<Button type="submit" loading={savingToken} disabled={newToken.trim() === ''}>Replace</Button>
			</form>

			<form class="setting" onsubmit={setSecret}>
				<Field label={app.has_client_secret ? 'Replace client secret' : 'Set client secret'} for="app-secret" hint={app.has_client_secret ? 'A secret is set. Enter a new one to replace it.' : 'Needed for Discord login through this application.'}>
					<PasswordInput id="app-secret" bind:value={newSecret} mono />
				</Field>
				<div class="row">
					<Button type="submit" loading={savingSecret} disabled={newSecret.trim() === ''}>{app.has_client_secret ? 'Replace' : 'Set'}</Button>
					{#if app.has_client_secret}
						<Button variant="danger-soft" loading={removingSecret} onclick={removeSecret}>Remove</Button>
					{/if}
				</div>
			</form>
		</div>
	</section>

	<section class="card card-danger">
		<div class="card-header"><h2>Remove application</h2></div>
		<div class="card-body row-between">
			<p class="muted">Retires the bot and removes every watch rule and guild record of this application from DiscoClip.</p>
			<Button variant="danger" icon="trash" loading={deleting} onclick={remove}>Remove application</Button>
		</div>
	</section>
</div>

<style>
	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.install {
		display: grid;
		grid-template-columns: 1fr auto auto;
		gap: 8px;
		align-items: center;
	}

	.modes {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 12px;
	}

	.commands {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}

	.command {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}

	.setting {
		display: grid;
		grid-template-columns: minmax(0, 480px) auto;
		gap: 12px;
		align-items: end;
	}

	@media (max-width: 640px) {
		.install,
		.setting {
			grid-template-columns: 1fr;
		}
	}
</style>
