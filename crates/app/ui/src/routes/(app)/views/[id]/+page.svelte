<script lang="ts">
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import XIcon from '@lucide/svelte/icons/x';
	import { SegmentedControl, Switch, TagsInput } from '@skeletonlabs/skeleton-svelte';
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
		Profile,
		ProviderInfo,
		SecretKind,
		Snowflake,
		ViewerSession
	} from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import ScopePicker from '$lib/components/guild/ScopePicker.svelte';
	import { number } from '$lib/format';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? 'new');
	const isNew = $derived(id === 'new');

	const SECRET_KINDS: [SecretKind, string][] = [
		['pin', 'PIN'],
		['password', 'Password'],
		['token', 'Access token']
	];

	let view = $state<Frontend | null>(null);
	let profiles = $state<Profile[]>([]);
	let providers = $state<ProviderInfo[]>([]);
	let botGuilds = $state<{ guild: BotGuild; applicationId: string }[]>([]);
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

	// The secret, set apart from the form: at once on a saved view, and right after
	// creating a new one.
	let secret = $state('');
	let secretPending = $state(false);
	let confirmClearSecret = $state(false);

	// Accounts and sessions
	let users = $state<FrontendUser[]>([]);
	let sessions = $state<ViewerSession[]>([]);
	let userDialog = $state(false);
	let newUsername = $state('');
	let newPassword = $state('');
	let userPending = $state(false);
	let passwordFor = $state<FrontendUser | null>(null);
	let deletingUser = $state<FrontendUser | null>(null);
	let deleteUserOpen = $state(false);
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

	const slugify = (text: string) =>
		text
			.toLowerCase()
			.replace(/[^a-z0-9]+/g, '-')
			.replace(/^-+|-+$/g, '');

	$effect(() => {
		if (!slugTouched) slug = slugify(name);
	});

	function toggle(list: string[], value: string): string[] {
		return list.includes(value) ? list.filter((v) => v !== value) : [...list, value];
	}

	const secretPlaceholder = $derived(
		secretKind === 'pin' ? 'New PIN' : secretKind === 'token' ? 'New access token' : 'New password'
	);

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
		if (!open && secretKind && isNew && !secret) {
			found.secret = `Enter the ${secretKind === 'pin' ? 'PIN' : secretKind === 'token' ? 'token' : 'password'} viewers will use.`;
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
				if (secret) await frontends.setSecret(created.id, secret);
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

	function askDeleteUser(user: FrontendUser) {
		deletingUser = user;
		deleteUserOpen = true;
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
			class: 'max-w-48 truncate text-xs text-surface-600-400'
		},
		{ key: 'seen', label: 'Last seen', cell: sessionSeen },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: sessionActions, align: 'right' }
	];

	const back = { href: resolve('/views'), label: 'Views' };
</script>

{#snippet userCreated(user: FrontendUser)}
	<Timestamp at={user.created_at} class="whitespace-nowrap" />
{/snippet}
{#snippet userActions(user: FrontendUser)}
	<span class="flex justify-end gap-1">
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={() => {
				passwordFor = user;
				newPassword = '';
				userDialog = true;
			}}>Set password</button
		>
		<button type="button" class="btn preset-tonal-error btn-sm" onclick={() => askDeleteUser(user)}
			>Remove</button
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
	<Timestamp at={s.last_seen_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionActions(s: ViewerSession)}
	<button
		type="button"
		class="btn preset-tonal-error btn-sm"
		onclick={() => revoke(s)}
		disabled={revoking !== null}
	>
		{#if revoking === s.id}<Spinner />{/if}
		End
	</button>
{/snippet}

{#snippet toggleSwitch(label: string, checked: boolean, onchange: (checked: boolean) => void)}
	<Switch {checked} onCheckedChange={(details) => onchange(details.checked)}>
		<Switch.Control><Switch.Thumb /></Switch.Control>
		<Switch.Label>{label}</Switch.Label>
		<Switch.HiddenInput />
	</Switch>
{/snippet}

{#if error && !loading}
	<PageHeader title="Content view" {back} />
	<ErrorState {error} title="This view could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Content view" {back} />
	<div class="space-y-3" aria-busy="true">
		<div class="h-8 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader
		title={isNew ? 'New content view' : (view?.name ?? 'Content view')}
		{back}
		description={isNew
			? 'A page that shows finished media to people outside this app, at its own address.'
			: undefined}
	>
		{#if view}
			<div class="flex flex-wrap items-center gap-3 text-sm">
				<Status enabled={view.enabled} />
				<span class="inline-flex min-w-0 items-center gap-1">
					<a
						href={shareUrl}
						class="truncate anchor font-mono text-xs"
						target="_blank"
						rel="noreferrer">{shareUrl} <ExternalLinkIcon class="inline size-3" /></a
					>
					<CopyButton text={shareUrl} label="Copy share link" />
				</span>
			</div>
		{/if}
	</PageHeader>

	<div class="grid items-start gap-6 {view ? 'xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]' : ''}">
		<form id="view-form" class="grid gap-6" onsubmit={save}>
			<Card title="General">
				<div class="grid gap-4 md:grid-cols-2">
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
							pattern="[a-z0-9\-]+"
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
					<div class="flex flex-col justify-center gap-3 md:pt-6">
						{@render toggleSwitch('Enabled', enabled, (value) => (enabled = value))}
						{@render toggleSwitch(
							'Viewers may download files',
							downloads,
							(value) => (downloads = value)
						)}
					</div>
				</div>
			</Card>

			<Card
				title="What it shows"
				description="Tick a server to show every channel in it, including channels made later. Open it and untick channels to narrow. With nothing ticked, every finished job shows."
			>
				<ScopePicker servers={botGuilds} bind:guilds={scopeGuilds} bind:channels={scopeChannels} />
			</Card>

			<Card title="Who gets in">
				<div class="space-y-4">
					<!-- Skeleton's SegmentedControl: open to all, or only after a login. -->
					<SegmentedControl
						value={open ? 'open' : 'login'}
						onValueChange={(details) => {
							if (details.value === 'open' || details.value === 'login')
								open = details.value === 'open';
						}}
						class="w-full"
					>
						<SegmentedControl.Label class="sr-only">Access</SegmentedControl.Label>
						<SegmentedControl.Control class="flex-wrap">
							<SegmentedControl.Indicator />
							<SegmentedControl.Item value="open">
								<SegmentedControl.ItemText>Anyone with the link</SegmentedControl.ItemText>
								<SegmentedControl.ItemHiddenInput />
							</SegmentedControl.Item>
							<SegmentedControl.Item value="login">
								<SegmentedControl.ItemText>Only people who log in</SegmentedControl.ItemText>
								<SegmentedControl.ItemHiddenInput />
							</SegmentedControl.Item>
						</SegmentedControl.Control>
					</SegmentedControl>

					{#if !open}
						<hr class="hr" />
						<div class="grid gap-4 md:grid-cols-2">
							<div class="space-y-4">
								<Field
									label="Shared secret"
									for="view-secret-kind"
									help="One code every viewer enters. Its kind decides how the login page asks for it."
								>
									<select id="view-secret-kind" class="select" bind:value={secretKind}>
										<option value="">Not used</option>
										{#each SECRET_KINDS as [value, label] (value)}
											<option {value}>{label}</option>
										{/each}
									</select>
								</Field>
								{#if secretKind}
									<div class="space-y-1">
										<div
											class="field-group {view
												? view.has_secret
													? 'grid-cols-[1fr_auto_auto]'
													: 'grid-cols-[1fr_auto]'
												: 'grid-cols-1'}"
										>
											<input
												class="input font-mono"
												type={secretKind === 'pin' ? 'text' : 'password'}
												inputmode={secretKind === 'pin' ? 'numeric' : undefined}
												placeholder={secretPlaceholder}
												bind:value={secret}
												aria-label={secretPlaceholder}
												autocomplete="off"
											/>
											{#if view}
												<button
													type="button"
													class="btn preset-filled"
													onclick={() => setSecret(secret)}
													disabled={secretPending || !secret}
												>
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
											{/if}
										</div>
										{#if errors.secret}
											<p class="text-xs text-error-600-400" role="alert">{errors.secret}</p>
										{:else if view}
											<p class="text-xs text-surface-600-400">
												{view.has_secret
													? 'A secret is set. Set takes effect at once and replaces it.'
													: 'No secret is set yet. Set takes effect at once.'}
											</p>
										{:else}
											<p class="text-xs text-surface-600-400">Stored when the view is created.</p>
										{/if}
									</div>
								{/if}
								{@render toggleSwitch(
									'View accounts may log in',
									accounts,
									(value) => (accounts = value)
								)}
							</div>
							<div class="space-y-4">
								<fieldset class="fieldset space-y-2">
									<legend class="legend">Login providers</legend>
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
											/>
											{provider.name}
										</label>
									{/each}
								</fieldset>
								{#if accessProviders.includes('discord')}
									{@render toggleSwitch(
										'A Discord login must belong to every server in the scope',
										discordMembers,
										(value) => (discordMembers = value)
									)}
									<Field
										label="Allowed Discord users"
										for="view-discord-users"
										help="User ids. Leave empty to allow any Discord login."
										error={errors.discordUsers}
									>
										<!-- Skeleton's TagsInput: one id per tag, checked as it is typed. -->
										<TagsInput
											value={discordUsers}
											onValueChange={(details) => (discordUsers = details.value)}
											validate={(details) =>
												/^\d{5,25}$/.test(details.inputValue) &&
												!details.value.includes(details.inputValue)}
										>
											<TagsInput.Control>
												<TagsInput.Context>
													{#snippet children(tagsInput)}
														{#each tagsInput().value as value, index (value)}
															<TagsInput.Item {value} {index}>
																<TagsInput.ItemPreview class="font-mono">
																	<TagsInput.ItemText>{value}</TagsInput.ItemText>
																	<TagsInput.ItemDeleteTrigger aria-label="Remove {value}">
																		<XIcon />
																	</TagsInput.ItemDeleteTrigger>
																</TagsInput.ItemPreview>
																<TagsInput.ItemInput class="font-mono" />
															</TagsInput.Item>
														{/each}
													{/snippet}
												</TagsInput.Context>
												<TagsInput.Input
													id="view-discord-users"
													class="font-mono"
													placeholder="Add a user id and press Enter"
												/>
											</TagsInput.Control>
											<TagsInput.HiddenInput />
										</TagsInput>
									</Field>
								{/if}
							</div>
						</div>
					{/if}
				</div>
			</Card>

			<Card
				title="Discord links"
				description="Post a page from this view instead of the file when an upload is too large or the output falls below these thresholds. Pages are built on the address this app is reached at, or web.public_url when set."
			>
				<div class="space-y-4">
					{@render toggleSwitch('Post links', linksEnabled, (value) => (linksEnabled = value))}
					<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
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
				</div>
			</Card>

			<div
				class="sticky bottom-[calc(5rem+env(safe-area-inset-bottom))] z-20 flex flex-wrap items-center justify-end gap-3 card preset-filled-surface-100-900 p-3 shadow-lg lg:bottom-4"
			>
				<a href={resolve('/views')} class="btn preset-tonal">Cancel</a>
				<button type="submit" class="btn preset-filled-primary-500" disabled={saving}>
					{#if saving}<Spinner />{/if}
					{isNew ? 'Create view' : 'Save view'}
				</button>
			</div>
		</form>

		{#if view}
			<div class="grid gap-6">
				<Card title="View accounts" count={number(users.length)} flush>
					{#snippet actions()}
						<button
							type="button"
							class="btn preset-tonal btn-sm"
							onclick={() => {
								passwordFor = null;
								newUsername = '';
								newPassword = '';
								userDialog = true;
							}}>Add account</button
						>
					{/snippet}
					<DataTable rows={users} columns={userColumns} rowKey={(u) => u.id} flush class="p-2">
						{#snippet empty()}
							No accounts. They exist only on this view.
						{/snippet}
					</DataTable>
				</Card>

				<Card title="Viewer sessions" count={number(sessions.length)} flush>
					{#snippet actions()}
						{#if sessions.length > 0}
							<button
								type="button"
								class="btn preset-tonal-error btn-sm"
								onclick={() => (confirmRevokeAll = true)}>End all</button
							>
						{/if}
					{/snippet}
					<DataTable
						rows={sessions}
						columns={sessionColumns}
						rowKey={(s) => s.id}
						flush
						class="p-2"
					>
						{#snippet empty()}
							Nobody is logged in to this view.
						{/snippet}
					</DataTable>
				</Card>
			</div>
		{/if}
	</div>
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
	bind:open={deleteUserOpen}
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
