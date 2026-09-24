<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import { TagsInput } from '@skeletonlabs/skeleton-svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import {
		applications,
		frontends,
		profiles as profilesApi,
		providers as providersApi
	} from '$lib/api/endpoints';
	import type {
		BotGuild,
		Frontend,
		FrontendInput,
		FrontendUser,
		GuildChannel,
		Profile,
		ProviderInfo,
		SecretKind,
		Snowflake,
		ViewerSession
	} from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { channelLabel, watchable } from '$lib/components/guild/channels';
	import { number } from '$lib/format';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? 'new');
	const isNew = $derived(id === 'new');

	let view = $state<Frontend | null>(null);
	let profiles = $state<Profile[]>([]);
	let providers = $state<ProviderInfo[]>([]);
	let botGuilds = $state<{ guild: BotGuild; applicationId: string }[]>([]);
	let channelsByGuild = $state<Record<Snowflake, GuildChannel[]>>({});
	let loading = $state(true);
	let error = $state<unknown>(null);
	let saving = $state(false);
	let errors = $state<Record<string, string>>({});

	// General
	let name = $state('');
	let slug = $state('');
	let slugTouched = $state(false);
	let description = $state('');
	let enabled = $state(true);
	let profileId = $state('');
	let downloads = $state(true);
	// Scope
	let scopeGuilds = $state<Snowflake[]>([]);
	let scopeChannels = $state<Snowflake[]>([]);
	let pickGuild = $state('');
	let loadingChannels = $state(false);
	// Access
	let open = $state(false);
	let secretKind = $state<SecretKind | ''>('');
	let accounts = $state(false);
	let accessProviders = $state<string[]>([]);
	let discordMembers = $state(false);
	let discordUsers = $state<Snowflake[]>([]);
	// Links
	let linksEnabled = $state(false);
	let minHeight = $state('720');
	let minBitrateKbps = $state('1500');
	let maxMb = $state(String(2048));
	let signedDays = $state('30');

	// Panels
	let secret = $state('');
	let secretPending = $state(false);
	let confirmClearSecret = $state(false);
	let users = $state<FrontendUser[]>([]);
	let sessions = $state<ViewerSession[]>([]);
	let userDialog = $state(false);
	let newUsername = $state('');
	let newPassword = $state('');
	let userPending = $state(false);
	let passwordFor = $state<FrontendUser | null>(null);
	let deletingUser = $state<FrontendUser | null>(null);
	let confirmRevokeAll = $state(false);
	let revoking = $state<string | null>(null);

	function fill(source: Frontend | null) {
		name = source?.name ?? '';
		slug = source?.slug ?? '';
		slugTouched = source !== null;
		description = source?.description ?? '';
		enabled = source?.enabled ?? true;
		profileId = source?.profile_id ?? '';
		downloads = source?.downloads ?? true;
		scopeGuilds = [...(source?.scope?.guilds ?? [])];
		scopeChannels = [...(source?.scope?.channels ?? [])];
		const a = source?.access;
		open = a?.open ?? false;
		secretKind = a?.secret_kind ?? '';
		accounts = a?.accounts ?? false;
		accessProviders = [...(a?.providers ?? [])];
		discordMembers = a?.discord_members ?? false;
		discordUsers = [...(a?.discord_users ?? [])];
		const l = source?.links;
		linksEnabled = l?.enabled ?? false;
		minHeight = String(l?.min_height ?? 720);
		minBitrateKbps = String(Math.round((l?.min_bitrate ?? 1_500_000) / 1000));
		maxMb = String(Math.round((l?.max_bytes ?? 2 * 1024 * 1024 * 1024) / 1024 / 1024));
		signedDays = String(l?.signed_link_days ?? 30);
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [loaded, profileList, providerList, apps] = await Promise.all([
				isNew ? Promise.resolve(null) : frontends.get(id),
				profilesApi.list(),
				providersApi.list(),
				applications.list()
			]);
			const guildLists = await Promise.all(
				apps.map(async (app) =>
					(await applications.guilds(app.id)).map((guild) => ({ guild, applicationId: app.id }))
				)
			);
			if (current !== requestId) return;
			view = loaded;
			profiles = profileList;
			providers = providerList;
			const seen = new SvelteSet<Snowflake>();
			botGuilds = guildLists.flat().filter(({ guild }) => {
				if (seen.has(guild.guild_id)) return false;
				seen.add(guild.guild_id);
				return true;
			});
			fill(loaded);
			if (loaded && (loaded.scope?.channels?.length ?? 0) > 0) {
				await Promise.all(botGuilds.map((entry) => loadChannels(entry.guild.guild_id)));
			}
			if (!isNew && loaded) await loadPanels();
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	async function loadPanels() {
		[users, sessions] = await Promise.all([frontends.users(id), frontends.sessions(id)]);
	}

	$effect(() => {
		void id;
		void load();
	});

	async function loadChannels(guild: Snowflake) {
		if (channelsByGuild[guild]) return;
		const entry = botGuilds.find((e) => e.guild.guild_id === guild);
		if (!entry) return;
		try {
			const list = await applications.channels(entry.applicationId, guild);
			channelsByGuild = { ...channelsByGuild, [guild]: list };
		} catch (err) {
			reportError(err, `Could not list the channels of ${entry.guild.name}`);
		}
	}

	$effect(() => {
		if (!pickGuild) return;
		loadingChannels = true;
		void loadChannels(pickGuild).finally(() => (loadingChannels = false));
	});

	const slugify = (text: string) =>
		text
			.toLowerCase()
			.replace(/[^a-z0-9]+/g, '-')
			.replace(/^-+|-+$/g, '');

	$effect(() => {
		if (!slugTouched) slug = slugify(name);
	});

	function channelName(channelId: Snowflake): string {
		for (const [guild, list] of Object.entries(channelsByGuild)) {
			const channel = list.find((c) => c.id === channelId);
			if (channel) {
				const guildName = botGuilds.find((e) => e.guild.guild_id === guild)?.guild.name ?? guild;
				return `${guildName} / ${channelLabel(channel)}`;
			}
		}
		return channelId;
	}

	function toggle(list: Snowflake[], value: Snowflake): Snowflake[] {
		return list.includes(value) ? list.filter((v) => v !== value) : [...list, value];
	}

	function build(): FrontendInput | null {
		const found: Record<string, string> = {};
		if (!name.trim()) found.name = 'Give the view a name.';
		if (!/^[a-z0-9-]+$/.test(slug)) found.slug = 'Lower-case letters, digits and dashes only.';
		const heightPx = Number(minHeight);
		const kbps = Number(minBitrateKbps);
		const mb = Number(maxMb);
		const days = Number(signedDays);
		if (linksEnabled) {
			if (!Number.isInteger(heightPx) || heightPx <= 0)
				found.minHeight = 'Whole pixels above zero.';
			if (!Number.isFinite(kbps) || kbps <= 0) found.minBitrate = 'A rate above zero.';
			if (!Number.isFinite(mb) || mb <= 0) found.maxMb = 'A size above zero.';
		}
		if (!Number.isInteger(days) || days <= 0) found.signedDays = 'Whole days above zero.';
		for (const user of discordUsers) {
			if (!/^\d{5,25}$/.test(user)) found.discordUsers = `${user} is not a Discord user id.`;
		}
		errors = found;
		if (Object.keys(found).length > 0) return null;
		return {
			name: name.trim(),
			slug,
			description: description.trim(),
			enabled,
			profile_id: profileId || undefined,
			scope: { guilds: scopeGuilds, channels: scopeChannels },
			access: {
				open,
				secret_kind: secretKind || null,
				accounts,
				providers: accessProviders,
				discord_members: discordMembers,
				discord_users: discordUsers
			},
			downloads,
			links: {
				enabled: linksEnabled,
				min_height: heightPx,
				min_bitrate: Math.round(kbps * 1000),
				max_bytes: Math.round(mb * 1024 * 1024),
				signed_link_days: days
			}
		};
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		const input = build();
		if (!input) return;
		saving = true;
		try {
			if (isNew) {
				const created = await frontends.create(input);
				notify.success('View created', created.name);
				await goto(resolve('/(app)/views/[id]', { id: created.id }));
			} else {
				view = await frontends.update(id, input);
				fill(view);
				notify.success('View saved', view.name);
			}
		} catch (err) {
			reportError(err, 'Could not save the view');
		} finally {
			saving = false;
		}
	}

	async function setSecret(value: string | null) {
		secretPending = true;
		try {
			view = await frontends.setSecret(id, value);
			secret = '';
			notify.success(value === null ? 'Shared secret removed' : 'Shared secret set');
		} catch (err) {
			reportError(err, 'Could not change the secret');
		} finally {
			secretPending = false;
		}
	}

	async function createUser(event: SubmitEvent) {
		event.preventDefault();
		userPending = true;
		try {
			if (passwordFor) {
				await frontends.setUserPassword(id, passwordFor.id, newPassword);
				notify.success('Password set', passwordFor.username);
			} else {
				const created = await frontends.createUser(id, {
					username: newUsername.trim(),
					password: newPassword
				});
				notify.success('Account created', created.username);
			}
			userDialog = false;
			newUsername = '';
			newPassword = '';
			passwordFor = null;
			await loadPanels();
		} catch (err) {
			reportError(err, 'Could not save the account');
		} finally {
			userPending = false;
		}
	}

	async function deleteUser() {
		if (!deletingUser) return;
		await frontends.removeUser(id, deletingUser.id);
		notify.success('Account removed', deletingUser.username);
		deletingUser = null;
		await loadPanels();
	}

	async function revoke(session: ViewerSession) {
		revoking = session.id;
		try {
			await frontends.revokeSession(id, session.id);
			sessions = sessions.filter((s) => s.id !== session.id);
		} catch (err) {
			reportError(err, 'Could not end the session');
		} finally {
			revoking = null;
		}
	}

	async function revokeAll() {
		await frontends.revokeSessions(id);
		sessions = [];
		notify.success('All viewer sessions ended');
	}

	const shareUrl = $derived(view ? `${location.origin}/f/${view.slug}` : '');

	const userColumns: Column<FrontendUser>[] = [
		{ key: 'username', label: 'Username', value: (u) => u.username, sortable: true },
		{ key: 'created', label: 'Created', cell: userCreated },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: userActions, align: 'right' }
	];
	const sessionColumns: Column<ViewerSession>[] = [
		{ key: 'display', label: 'Viewer', cell: sessionWho },
		{ key: 'ip', label: 'Address', value: (s) => s.ip, class: 'font-mono text-xs' },
		{
			key: 'agent',
			label: 'Browser',
			value: (s) => s.user_agent,
			class: 'max-w-48 truncate text-sm'
		},
		{ key: 'seen', label: 'Last seen', cell: sessionSeen },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: sessionActions, align: 'right' }
	];
</script>

{#snippet userCreated(user: FrontendUser)}
	<RelativeTime at={user.created_at} class="whitespace-nowrap" />
{/snippet}
{#snippet userActions(user: FrontendUser)}
	<span class="flex justify-end gap-1">
		<button
			type="button"
			class="btn btn-sm hover:preset-tonal"
			onclick={() => {
				passwordFor = user;
				newPassword = '';
				userDialog = true;
			}}>Set password</button
		>
		<button
			type="button"
			class="btn btn-sm hover:preset-tonal-error"
			onclick={() => (deletingUser = user)}>Remove</button
		>
	</span>
{/snippet}
{#snippet sessionWho(s: ViewerSession)}
	<div class="min-w-0">
		<p class="truncate font-medium">{s.display}</p>
		<p class="truncate font-mono text-xs text-surface-600-400">{s.subject}</p>
	</div>
{/snippet}
{#snippet sessionSeen(s: ViewerSession)}
	<RelativeTime at={s.last_seen_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionActions(s: ViewerSession)}
	<button
		type="button"
		class="btn btn-sm hover:preset-tonal-error"
		onclick={() => revoke(s)}
		disabled={revoking !== null}
	>
		{#if revoking === s.id}<Spinner />{/if}
		End
	</button>
{/snippet}

{#if error && !loading}
	<PageHeader title="Content view" />
	<ErrorState {error} title="This view could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Content view" />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<a
		href={resolve('/views')}
		class="link-body inline-flex items-center gap-1 text-sm text-surface-700-300 hover:text-surface-950-50"
	>
		<ArrowLeftIcon class="size-4" />
		All views
	</a>

	<PageHeader title={isNew ? 'New content view' : (view?.name ?? 'Content view')}>
		{#if view}
			<p class="flex flex-wrap items-center gap-2 text-sm text-surface-600-400">
				<a href={shareUrl} class="anchor font-mono text-xs" target="_blank" rel="noreferrer"
					>{shareUrl} <ExternalLinkIcon class="inline size-3" /></a
				>
				<CopyButton text={shareUrl} label="Copy share link" />
			</p>
		{/if}
	</PageHeader>

	<form class="space-y-6" onsubmit={save}>
		<section
			class="grid gap-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6 md:grid-cols-2"
			aria-label="General"
		>
			<h2 class="h6 md:col-span-2">General</h2>
			<Field label="Name" for="view-name" required error={errors.name}>
				<input id="view-name" class="input" type="text" bind:value={name} required />
			</Field>
			<Field
				label="Slug"
				for="view-slug"
				required
				help="The address: /f/<slug>"
				error={errors.slug}
			>
				<input
					id="view-slug"
					class="input font-mono"
					type="text"
					bind:value={slug}
					oninput={() => (slugTouched = true)}
					required
					pattern="[a-z0-9-]+"
				/>
			</Field>
			<Field label="Description" for="view-description" class="md:col-span-2">
				<input id="view-description" class="input" type="text" bind:value={description} />
			</Field>
			<Field
				label="Profile"
				for="view-profile"
				help="Only media from platforms the profile enables is shown."
			>
				<select id="view-profile" class="select" bind:value={profileId}>
					<option value="">Built-in default</option>
					{#each profiles as profile (profile.id)}
						<option value={profile.id}>{profile.name}</option>
					{/each}
				</select>
			</Field>
			<div class="space-y-2 self-end">
				<label class="flex items-center gap-2 text-sm">
					<input class="checkbox" type="checkbox" bind:checked={enabled} />
					Enabled
				</label>
				<label class="flex items-center gap-2 text-sm">
					<input class="checkbox" type="checkbox" bind:checked={downloads} />
					Allow downloads
				</label>
			</div>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Scope"
		>
			<h2 class="h6">What it shows</h2>
			<p class="text-sm text-surface-600-400">
				Media from any listed server or channel. With nothing picked, every finished job shows.
			</p>
			<div class="grid gap-4 md:grid-cols-2">
				<fieldset class="space-y-1">
					<legend class="label-text">Servers</legend>
					{#if botGuilds.length === 0}
						<p class="text-sm text-surface-600-400">No bot is in a server yet.</p>
					{/if}
					<div class="max-h-56 space-y-1 overflow-y-auto">
						{#each botGuilds as { guild } (guild.guild_id)}
							<label
								class="flex items-center gap-2 rounded-base px-1 py-0.5 text-sm hover:bg-surface-200-800"
							>
								<input
									class="checkbox"
									type="checkbox"
									checked={scopeGuilds.includes(guild.guild_id)}
									onchange={() => (scopeGuilds = toggle(scopeGuilds, guild.guild_id))}
								/>
								<GuildIcon guild={guild.guild_id} hash={guild.icon} name={guild.name} size={20} />
								<span class="truncate">{guild.name}</span>
								{#if !guild.present}<span class="text-surface-600-400">left</span>{/if}
							</label>
						{/each}
						{#each scopeGuilds.filter((g) => !botGuilds.some((e) => e.guild.guild_id === g)) as unknown (unknown)}
							<label class="flex items-center gap-2 rounded-base px-1 py-0.5 text-sm">
								<input
									class="checkbox"
									type="checkbox"
									checked
									onchange={() => (scopeGuilds = toggle(scopeGuilds, unknown))}
								/>
								<span class="font-mono text-xs">{unknown}</span>
							</label>
						{/each}
					</div>
				</fieldset>
				<fieldset class="space-y-2">
					<legend class="label-text">Channels</legend>
					<select class="select" bind:value={pickGuild} aria-label="Server to pick channels from">
						<option value="">Pick a server…</option>
						{#each botGuilds as { guild } (guild.guild_id)}
							<option value={guild.guild_id}>{guild.name}</option>
						{/each}
					</select>
					{#if pickGuild}
						{#if loadingChannels && !channelsByGuild[pickGuild]}
							<p class="flex items-center gap-2 text-sm text-surface-600-400">
								<Spinner /> Loading channels…
							</p>
						{:else}
							<div
								class="max-h-40 space-y-1 overflow-y-auto rounded-base border border-surface-200-800 p-2"
							>
								{#each watchable(channelsByGuild[pickGuild] ?? []) as { channel, category } (channel.id)}
									<label class="flex items-center gap-2 text-sm">
										<input
											class="checkbox"
											type="checkbox"
											checked={scopeChannels.includes(channel.id)}
											onchange={() => (scopeChannels = toggle(scopeChannels, channel.id))}
										/>
										<span class="truncate"
											>{category ? `${category} / ` : ''}{channelLabel(channel)}</span
										>
									</label>
								{/each}
							</div>
						{/if}
					{/if}
					{#if scopeChannels.length > 0}
						<ul class="flex flex-wrap gap-1">
							{#each scopeChannels as channelId (channelId)}
								<li>
									<button
										type="button"
										class="chip preset-tonal"
										onclick={() => (scopeChannels = toggle(scopeChannels, channelId))}
										title="Remove"
									>
										{channelName(channelId)} ×
									</button>
								</li>
							{/each}
						</ul>
					{/if}
				</fieldset>
			</div>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Access"
		>
			<h2 class="h6">Who gets in</h2>
			<label class="flex items-center gap-2 text-sm">
				<input class="checkbox" type="checkbox" bind:checked={open} />
				Open to everyone, no login
			</label>
			<div class="grid gap-4 md:grid-cols-2" class:opacity-50={open}>
				<Field
					label="Shared secret"
					for="view-secret-kind"
					help={view?.has_secret ? 'A secret is set below.' : 'Set the secret below after saving.'}
				>
					<select id="view-secret-kind" class="select" bind:value={secretKind} disabled={open}>
						<option value="">None</option>
						<option value="pin">PIN</option>
						<option value="password">Password</option>
						<option value="token">Access token</option>
					</select>
				</Field>
				<div class="space-y-2 self-end">
					<label class="flex items-center gap-2 text-sm">
						<input class="checkbox" type="checkbox" bind:checked={accounts} disabled={open} />
						View accounts may log in
					</label>
				</div>
				<fieldset class="space-y-1">
					<legend class="label-text">Login providers</legend>
					{#if providers.length === 0}
						<p class="text-sm text-surface-600-400">
							No provider is configured in Settings or Applications.
						</p>
					{/if}
					{#each providers as provider (provider.id)}
						<label class="flex items-center gap-2 text-sm">
							<input
								class="checkbox"
								type="checkbox"
								checked={accessProviders.includes(provider.id)}
								onchange={() => (accessProviders = toggle(accessProviders, provider.id))}
								disabled={open}
							/>
							{provider.name}
						</label>
					{/each}
				</fieldset>
				<div class="space-y-3">
					<label class="flex items-center gap-2 text-sm">
						<input
							class="checkbox"
							type="checkbox"
							bind:checked={discordMembers}
							disabled={open || !accessProviders.includes('discord')}
						/>
						A Discord login must belong to every server in the scope
					</label>
					<Field
						label="Allowed Discord users"
						for="view-discord-users"
						help="User ids. Leave empty to allow any Discord login."
						error={errors.discordUsers}
					>
						<TagsInput
							value={discordUsers}
							onValueChange={(details) => (discordUsers = details.value)}
							validate={(details) =>
								/^\d{5,25}$/.test(details.inputValue) &&
								!details.value.includes(details.inputValue)}
							disabled={open || !accessProviders.includes('discord')}
						>
							<TagsInput.Control class="input flex min-h-10 flex-wrap items-center gap-1 py-1">
								<TagsInput.Context>
									{#snippet children(tagsInput)}
										{#each tagsInput().value as value, index (value)}
											<TagsInput.Item {value} {index}>
												<TagsInput.ItemPreview class="chip preset-tonal font-mono text-xs">
													<TagsInput.ItemText>{value}</TagsInput.ItemText>
													<TagsInput.ItemDeleteTrigger aria-label="Remove {value}"
														>×</TagsInput.ItemDeleteTrigger
													>
												</TagsInput.ItemPreview>
												<TagsInput.ItemInput class="font-mono text-xs" />
											</TagsInput.Item>
										{/each}
									{/snippet}
								</TagsInput.Context>
								<TagsInput.Input
									id="view-discord-users"
									class="min-w-32 flex-1 bg-transparent font-mono text-xs outline-none"
									placeholder="Add a user id and press Enter"
								/>
							</TagsInput.Control>
							<TagsInput.HiddenInput />
						</TagsInput>
					</Field>
				</div>
			</div>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Discord links"
		>
			<h2 class="h6">Discord links</h2>
			<p class="text-sm text-surface-600-400">
				Post a page from this view instead of the file when an upload is too large or the output
				falls below these thresholds. Needs web.public_url in Settings.
			</p>
			<label class="flex items-center gap-2 text-sm">
				<input class="checkbox" type="checkbox" bind:checked={linksEnabled} />
				Post links
			</label>
			<div class="grid gap-4 md:grid-cols-4">
				<Field label="Min height" for="view-min-height" help="Pixels" error={errors.minHeight}>
					<input
						id="view-min-height"
						class="input"
						type="number"
						min="1"
						bind:value={minHeight}
						disabled={!linksEnabled}
					/>
				</Field>
				<Field label="Min bitrate" for="view-min-bitrate" help="kbps" error={errors.minBitrate}>
					<input
						id="view-min-bitrate"
						class="input"
						type="number"
						min="1"
						bind:value={minBitrateKbps}
						disabled={!linksEnabled}
					/>
				</Field>
				<Field label="Max page output" for="view-max-mb" help="MB" error={errors.maxMb}>
					<input
						id="view-max-mb"
						class="input"
						type="number"
						min="1"
						bind:value={maxMb}
						disabled={!linksEnabled}
					/>
				</Field>
				<Field
					label="Signed links last"
					for="view-signed-days"
					help="Days"
					error={errors.signedDays}
				>
					<input
						id="view-signed-days"
						class="input"
						type="number"
						min="1"
						bind:value={signedDays}
					/>
				</Field>
			</div>
		</section>

		<div class="flex justify-end gap-2">
			<a href={resolve('/views')} class="btn preset-tonal">Cancel</a>
			<button type="submit" class="btn preset-filled-primary-500" disabled={saving}>
				{#if saving}<Spinner />{/if}
				{isNew ? 'Create view' : 'Save view'}
			</button>
		</div>
	</form>

	{#if view}
		<div class="grid gap-6 lg:grid-cols-2">
			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Shared secret"
			>
				<h2 class="h6">Shared secret</h2>
				<p class="text-sm text-surface-600-400">
					{view.has_secret
						? 'A secret is set. It cannot be shown again; enter a new one to replace it.'
						: 'No secret is set.'}
					{#if !secretKind}Choose how it is asked for under Access.{/if}
				</p>
				<form
					class="flex gap-2"
					onsubmit={(event) => {
						event.preventDefault();
						void setSecret(secret);
					}}
				>
					<input
						class="input font-mono"
						type={secretKind === 'pin' ? 'text' : 'password'}
						inputmode={secretKind === 'pin' ? 'numeric' : undefined}
						placeholder={secretKind === 'pin'
							? 'New PIN'
							: secretKind === 'token'
								? 'New access token'
								: 'New password'}
						bind:value={secret}
						aria-label="New secret"
						autocomplete="off"
					/>
					<button type="submit" class="btn preset-filled" disabled={secretPending || !secret}>
						{#if secretPending}<Spinner />{/if}
						Set
					</button>
					{#if view.has_secret}
						<button
							type="button"
							class="btn preset-tonal-error"
							onclick={() => (confirmClearSecret = true)}
							disabled={secretPending}>Remove</button
						>
					{/if}
				</form>
			</section>

			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="View accounts"
			>
				<div class="flex items-center justify-between">
					<h2 class="h6">View accounts ({number(users.length)})</h2>
					<button
						type="button"
						class="btn preset-filled btn-sm"
						onclick={() => {
							passwordFor = null;
							newUsername = '';
							newPassword = '';
							userDialog = true;
						}}>Add account</button
					>
				</div>
				<DataTable rows={users} columns={userColumns} rowKey={(u) => u.id} dense>
					{#snippet empty()}
						No accounts. They exist only on this view.
					{/snippet}
				</DataTable>
			</section>
		</div>

		<section class="space-y-3" aria-label="Viewer sessions">
			<div class="flex items-center justify-between">
				<h2 class="h6">Viewer sessions ({number(sessions.length)})</h2>
				{#if sessions.length > 0}
					<button
						type="button"
						class="btn preset-tonal-error btn-sm"
						onclick={() => (confirmRevokeAll = true)}>End all</button
					>
				{/if}
			</div>
			<DataTable rows={sessions} columns={sessionColumns} rowKey={(s) => s.id} dense>
				{#snippet empty()}
					Nobody is logged in to this view.
				{/snippet}
			</DataTable>
		</section>
	{/if}
{/if}

<Modal
	bind:open={userDialog}
	title={passwordFor ? `Set the password of ${passwordFor.username}` : 'Add a view account'}
	busy={userPending}
>
	<form id="view-user-form" class="space-y-4" onsubmit={createUser}>
		{#if !passwordFor}
			<Field label="Username" for="view-user-name" required>
				<input
					id="view-user-name"
					class="input"
					type="text"
					bind:value={newUsername}
					required
					autocomplete="off"
				/>
			</Field>
		{/if}
		<Field label="Password" for="view-user-password" required>
			<input
				id="view-user-password"
				class="input"
				type="password"
				bind:value={newPassword}
				required
				autocomplete="new-password"
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (userDialog = false)}
			disabled={userPending}>Cancel</button
		>
		<button
			type="submit"
			form="view-user-form"
			class="btn preset-filled-primary-500"
			disabled={userPending}
		>
			{#if userPending}<Spinner />{/if}
			{passwordFor ? 'Set password' : 'Add account'}
		</button>
	{/snippet}
</Modal>

<Confirm
	bind:open={confirmClearSecret}
	title="Remove the shared secret?"
	message="Viewers relying on it can no longer log in."
	confirmLabel="Remove"
	danger
	onconfirm={() => setSecret(null)}
/>
<Confirm
	open={deletingUser !== null}
	title="Remove {deletingUser?.username ?? 'this account'}?"
	message="Its sessions end."
	confirmLabel="Remove"
	danger
	onconfirm={deleteUser}
/>
<Confirm
	bind:open={confirmRevokeAll}
	title="End every viewer session?"
	message="Everyone logged in to this view is logged out."
	confirmLabel="End all"
	danger
	onconfirm={revokeAll}
/>
