<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { Tabs } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import {
		providers as providersApi,
		sessions as sessionsApi,
		tokens as tokensApi,
		users as usersApi
	} from '$lib/api/endpoints';
	import type {
		ApiToken,
		CallbackError,
		Identity,
		Permission,
		ProviderInfo,
		SessionView
	} from '$lib/api/types';
	import { PERMISSION_LABELS, ROLE_PERMISSIONS } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import ChipList from '$lib/components/ChipList.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { countText, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	type Tab = 'password' | 'logins' | 'sessions' | 'tokens';

	let tab = $state<Tab>('password');
	let sessions = $state<SessionView[]>([]);
	let identities = $state<Identity[]>([]);
	let providers = $state<ProviderInfo[]>([]);
	let tokens = $state<ApiToken[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	let currentPassword = $state('');
	let newPassword = $state('');
	let confirmPassword = $state('');
	let changingPassword = $state(false);
	const mismatch = $derived(confirmPassword.length > 0 && confirmPassword !== newPassword);

	let revoking = $state<string | null>(null);
	let confirmOthers = $state(false);
	let identityPending = $state<string | null>(null);
	let unlinking = $state<Identity | null>(null);

	let tokenOpen = $state(false);
	let tokenName = $state('');
	let tokenScopes = $state<Permission[]>([]);
	let tokenDays = $state('');
	let minting = $state(false);
	let minted = $state<{ token: ApiToken; secret: string } | null>(null);
	let revokingToken = $state<ApiToken | null>(null);

	const CALLBACK_ERRORS: Record<CallbackError, string> = {
		state: 'The link could not be verified. Try again.',
		denied: 'The provider denied the request.',
		provider: 'The provider returned an error.',
		identity: 'The provider did not say who you are.',
		exchange: 'The provider did not accept the code.',
		session: 'Your session could not be found. Log in again.',
		already_linked: 'That provider account is linked to a different DiscoClip account.',
		provider_linked: 'Your account already has that provider linked.',
		unknown_identity: 'No account is linked to that provider login.'
	};
	const callbackError = $derived.by(() => {
		const code = page.url.searchParams.get('error');
		return code && code in CALLBACK_ERRORS ? CALLBACK_ERRORS[code as CallbackError] : null;
	});

	const allowedScopes = $derived(session.user ? ROLE_PERMISSIONS[session.user.role] : []);

	function isTab(value: string): value is Tab {
		return value === 'password' || value === 'logins' || value === 'sessions' || value === 'tokens';
	}

	async function load() {
		loading = true;
		error = null;
		try {
			[sessions, identities, providers, tokens] = await Promise.all([
				sessionsApi.list(),
				providersApi.identities(),
				providersApi.list(),
				tokensApi.list()
			]);
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	async function changePassword(event: SubmitEvent) {
		event.preventDefault();
		if (!session.user || mismatch) return;
		changingPassword = true;
		try {
			await usersApi.setPassword(session.user.id, {
				password: newPassword,
				current_password: currentPassword
			});
			notify.success('Password changed', 'Your other sessions ended');
			currentPassword = '';
			newPassword = '';
			confirmPassword = '';
			sessions = await sessionsApi.list();
			await session.refresh();
		} catch (err) {
			reportError(err, 'Could not change the password');
		} finally {
			changingPassword = false;
		}
	}

	async function endSession(s: SessionView) {
		revoking = s.id;
		try {
			await sessionsApi.revoke(s.id);
			if (s.current) {
				await session.expire();
				return;
			}
			sessions = sessions.filter((x) => x.id !== s.id);
		} catch (err) {
			reportError(err, 'Could not end the session');
		} finally {
			revoking = null;
		}
	}

	async function endOthers() {
		const result = await sessionsApi.revokeOthers();
		notify.success(`${countText(result.revoked, 'other session')} ended`);
		sessions = sessions.filter((s) => s.current);
	}

	async function refreshIdentity(identity: Identity) {
		identityPending = identity.provider;
		try {
			const updated = await providersApi.refresh(identity.provider);
			identities = identities.map((i) => (i.provider === updated.provider ? updated : i));
			notify.success(
				'Identity refreshed',
				updated.display_name ?? updated.username ?? updated.subject
			);
		} catch (err) {
			reportError(err, 'Could not refresh the identity');
		} finally {
			identityPending = null;
		}
	}

	async function unlink() {
		if (!unlinking) return;
		const result = await providersApi.unlink(unlinking.provider);
		notify.success(
			'Unlinked',
			result.revoked ? 'The grant at the provider was revoked too' : 'The provider kept its grant'
		);
		identities = identities.filter((i) => i.provider !== result.identity.provider);
		unlinking = null;
	}

	async function mint(event: SubmitEvent) {
		event.preventDefault();
		minting = true;
		try {
			const days = tokenDays.trim() ? Number(tokenDays) : undefined;
			const result = await tokensApi.create({
				name: tokenName.trim(),
				scopes: tokenScopes,
				expires_in_days: days !== undefined && Number.isInteger(days) && days > 0 ? days : undefined
			});
			tokens = [result.token, ...tokens];
			minted = result;
			tokenOpen = false;
			tokenName = '';
			tokenScopes = [];
			tokenDays = '';
		} catch (err) {
			reportError(err, 'Could not create the token');
		} finally {
			minting = false;
		}
	}

	async function revokeToken() {
		if (!revokingToken) return;
		await tokensApi.revoke(revokingToken.id);
		tokens = tokens.filter((t) => t.id !== revokingToken?.id);
		notify.success('Token revoked', revokingToken.name);
		revokingToken = null;
	}

	function toggleScope(scope: Permission) {
		tokenScopes = tokenScopes.includes(scope)
			? tokenScopes.filter((s) => s !== scope)
			: [...tokenScopes, scope];
	}

	const linkedProviders = $derived(new Set(identities.map((i) => i.provider)));
	const unlinkedProviders = $derived(providers.filter((p) => !linkedProviders.has(p.id)));

	const sessionColumns: Column<SessionView>[] = [
		{ key: 'where', label: 'Session', cell: sessionCell },
		{ key: 'created', label: 'Started', cell: sessionCreated },
		{ key: 'seen', label: 'Last seen', cell: sessionSeen },
		{ key: 'expires', label: 'Expires', cell: sessionExpires },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: sessionActions, align: 'right' }
	];
	const tokenColumns: Column<ApiToken>[] = [
		{ key: 'name', label: 'Token', cell: tokenCell },
		{ key: 'scopes', label: 'Scopes', cell: scopesCell },
		{ key: 'used', label: 'Last used', cell: tokenUsed },
		{ key: 'expires', label: 'Expires', cell: tokenExpires },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: tokenActions, align: 'right' }
	];
</script>

{#snippet sessionCell(s: SessionView)}
	<div class="min-w-0">
		<p class="flex flex-wrap items-center gap-2">
			<span class="font-mono text-xs">{s.ip ?? 'unknown address'}</span>
			{#if s.current}
				<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">This session</span>
			{/if}
		</p>
		<p class="truncate text-sm text-surface-600-400" title={s.user_agent ?? ''}>
			{s.user_agent ?? 'unknown browser'}
		</p>
	</div>
{/snippet}
{#snippet sessionCreated(s: SessionView)}
	<Timestamp at={s.created_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionSeen(s: SessionView)}
	<Timestamp at={s.last_seen_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionExpires(s: SessionView)}
	<Timestamp at={s.expires_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionActions(s: SessionView)}
	<button
		type="button"
		class="btn preset-tonal-error btn-sm"
		onclick={() => endSession(s)}
		disabled={revoking !== null}
	>
		{#if revoking === s.id}<Spinner />{/if}
		{s.current ? 'Log out' : 'End'}
	</button>
{/snippet}
{#snippet tokenCell(t: ApiToken)}
	<span class="font-medium">{t.name}</span>
	<span class="ml-2 font-mono text-xs text-surface-600-400">{t.prefix}…</span>
{/snippet}
{#snippet scopesCell(t: ApiToken)}
	<ChipList items={t.scopes.map((scope) => PERMISSION_LABELS[scope])} />
{/snippet}
{#snippet tokenUsed(t: ApiToken)}
	<Timestamp at={t.last_used_at} class="whitespace-nowrap" />
{/snippet}
{#snippet tokenExpires(t: ApiToken)}
	{#if t.expires_at}
		<Timestamp at={t.expires_at} class="whitespace-nowrap" />
	{:else}
		<span class="text-surface-600-400">Never</span>
	{/if}
{/snippet}
{#snippet tokenActions(t: ApiToken)}
	<button type="button" class="btn preset-tonal-error btn-sm" onclick={() => (revokingToken = t)}>
		Revoke
	</button>
{/snippet}

<PageHeader title="Account">
	{#if session.user}
		<p class="flex flex-wrap items-center gap-2">
			<span class="font-medium">{session.user.username}</span>
			<span class="badge preset-tonal capitalize" style="--badge-size: var(--text-xs)">
				{session.user.role}
			</span>
		</p>
	{/if}
</PageHeader>

{#if callbackError}
	<p class="card preset-tonal-error p-3 text-sm" role="alert">{callbackError}</p>
{/if}

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<!-- Skeleton's Tabs: the account's password, logins, sessions and tokens. -->
	<Tabs
		value={tab}
		onValueChange={(details) => {
			if (isTab(details.value)) tab = details.value;
		}}
	>
		<Tabs.List class="overflow-x-auto">
			<Tabs.Trigger value="password">Password</Tabs.Trigger>
			<Tabs.Trigger value="logins">Linked logins</Tabs.Trigger>
			<Tabs.Trigger value="sessions" class="gap-2">
				Sessions
				<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
					{number(sessions.length)}
				</span>
			</Tabs.Trigger>
			<Tabs.Trigger value="tokens" class="gap-2">
				API tokens
				<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
					{number(tokens.length)}
				</span>
			</Tabs.Trigger>
			<Tabs.Indicator />
		</Tabs.List>

		<Tabs.Content value="password">
			<Card title="Password" class="max-w-xl">
				{#if session.user && !session.user.has_password}
					<p class="text-sm text-surface-600-400">
						Your account has no password; you log in through a provider. An admin can set one under
						Users.
					</p>
				{:else}
					<form class="space-y-4" onsubmit={changePassword}>
						<Field label="Current password" for="current-password" required>
							<input
								id="current-password"
								class="input"
								type="password"
								bind:value={currentPassword}
								autocomplete="current-password"
								required
							/>
						</Field>
						<Field label="New password" for="new-password" required>
							<input
								id="new-password"
								class="input"
								type="password"
								bind:value={newPassword}
								autocomplete="new-password"
								required
							/>
						</Field>
						<Field
							label="Confirm new password"
							for="confirm-password"
							required
							error={mismatch ? 'The passwords differ.' : null}
						>
							<input
								id="confirm-password"
								class="input"
								type="password"
								bind:value={confirmPassword}
								autocomplete="new-password"
								required
							/>
						</Field>
						<div class="flex justify-end">
							<button
								type="submit"
								class="btn preset-filled-primary-500"
								disabled={changingPassword || mismatch}
							>
								{#if changingPassword}<Spinner />{/if}
								Change password
							</button>
						</div>
					</form>
				{/if}
			</Card>
		</Tabs.Content>

		<Tabs.Content value="logins">
			<Card title="Linked logins" class="max-w-3xl">
				<div class="space-y-4">
					{#if loading && identities.length === 0}
						<div
							class="h-16 placeholder animate-pulse"
							aria-busy="true"
							aria-label="Loading logins"
						></div>
					{:else if identities.length === 0 && unlinkedProviders.length === 0}
						<p class="text-sm text-surface-600-400">
							No login provider is configured on this server.
						</p>
					{/if}
					{#if identities.length > 0}
						<ul class="space-y-3">
							{#each identities as identity (identity.provider)}
								{@const provider = providers.find((p) => p.id === identity.provider)}
								<li class="flex flex-wrap items-center justify-between gap-3 card preset-tonal p-3">
									<div class="min-w-0 space-y-0.5 text-sm">
										<p class="font-medium">{provider?.name ?? identity.provider}</p>
										<p class="truncate text-surface-600-400">
											{identity.display_name ?? identity.username ?? identity.subject}
											{#if identity.email}· {identity.email}{/if}
										</p>
										<p class="text-surface-600-400">
											Linked <Timestamp at={identity.linked_at} />
											{#if identity.expires_at}
												· token expires <Timestamp at={identity.expires_at} />
											{/if}
											{#if identity.has_refresh_token}· refreshable{/if}
										</p>
									</div>
									<div class="flex gap-2">
										<button
											type="button"
											class="btn preset-tonal btn-sm"
											onclick={() => refreshIdentity(identity)}
											disabled={identityPending !== null}
										>
											{#if identityPending === identity.provider}<Spinner />{/if}
											Refresh
										</button>
										<button
											type="button"
											class="btn preset-tonal-error btn-sm"
											onclick={() => (unlinking = identity)}>Unlink</button
										>
									</div>
								</li>
							{/each}
						</ul>
					{/if}
					{#if unlinkedProviders.length > 0}
						<div class="flex flex-wrap gap-2">
							{#each unlinkedProviders as provider (provider.id)}
								<a href={providersApi.startUrl(provider.id, 'link')} class="btn preset-tonal">
									Link {provider.name}
								</a>
							{/each}
						</div>
					{/if}
				</div>
			</Card>
		</Tabs.Content>

		<Tabs.Content value="sessions">
			<Card title="Sessions" flush>
				{#snippet actions()}
					{#if sessions.length > 1}
						<button
							type="button"
							class="btn preset-tonal btn-sm"
							onclick={() => (confirmOthers = true)}>End other sessions</button
						>
					{/if}
				{/snippet}
				<DataTable
					rows={sessions}
					columns={sessionColumns}
					rowKey={(s) => s.id}
					{loading}
					flush
					class="p-2"
				/>
			</Card>
		</Tabs.Content>

		<Tabs.Content value="tokens">
			<Card title="API tokens" flush>
				{#snippet actions()}
					<button
						type="button"
						class="btn preset-filled-primary-500 btn-sm"
						onclick={() => (tokenOpen = true)}
					>
						<PlusIcon class="size-4" />
						Create token
					</button>
				{/snippet}
				<p class="px-4 pt-3 text-sm text-surface-600-400">
					Send a token as <code>Authorization: Bearer dc_…</code>. It is limited to its scopes and
					to what your role allows.
				</p>
				<DataTable
					rows={tokens}
					columns={tokenColumns}
					rowKey={(t) => t.id}
					{loading}
					flush
					class="p-2"
				>
					{#snippet empty()}
						No tokens yet.
					{/snippet}
				</DataTable>
			</Card>
		</Tabs.Content>
	</Tabs>
{/if}

<Modal bind:open={tokenOpen} title="Create an API token" busy={minting}>
	<form id="mint-token" class="space-y-4" onsubmit={mint}>
		<Field label="Name" for="token-name" required help="What it is for, so you recognise it later.">
			<input
				id="token-name"
				class="input"
				type="text"
				bind:value={tokenName}
				required
				autocomplete="off"
			/>
		</Field>
		<fieldset class="fieldset space-y-2">
			<legend class="legend">Scopes</legend>
			{#if allowedScopes.length === 0}
				<p class="text-sm text-surface-600-400">
					Your role grants read access only, so the token reads only.
				</p>
			{/if}
			{#each allowedScopes as scope (scope)}
				<label class="flex items-center gap-2 text-sm">
					<input
						class="checkbox"
						type="checkbox"
						checked={tokenScopes.includes(scope)}
						onchange={() => toggleScope(scope)}
					/>
					<span title={scope}>{PERMISSION_LABELS[scope]}</span>
				</label>
			{/each}
		</fieldset>
		<Field
			label="Expires in"
			for="token-days"
			help="Days. Leave empty for a token that lasts until revoked."
		>
			<input id="token-days" class="input" type="number" min="1" bind:value={tokenDays} />
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (tokenOpen = false)}
			disabled={minting}>Cancel</button
		>
		<button
			type="submit"
			form="mint-token"
			class="btn preset-filled-primary-500"
			disabled={minting}
		>
			{#if minting}<Spinner />{/if}
			Create
		</button>
	{/snippet}
</Modal>

<Modal
	open={minted !== null}
	title="Copy your new token"
	description="This is the only time it is shown."
>
	{#if minted}
		<div class="space-y-3">
			<p class="text-sm font-medium">{minted.token.name}</p>
			<div class="flex items-center gap-2 card preset-tonal p-3">
				<span class="min-w-0 flex-1 font-mono text-sm break-all">{minted.secret}</span>
				<CopyButton text={minted.secret} label="Copy token" />
			</div>
		</div>
	{/if}
	{#snippet footer()}
		<button type="button" class="btn preset-filled-primary-500" onclick={() => (minted = null)}>
			Done
		</button>
	{/snippet}
</Modal>

<Confirm
	bind:open={confirmOthers}
	title="End your other sessions?"
	message="Every other browser logged in as you is logged out."
	confirmLabel="End others"
	onconfirm={endOthers}
/>
<Confirm
	open={unlinking !== null}
	title="Unlink {providers.find((p) => p.id === unlinking?.provider)?.name ??
		unlinking?.provider ??
		''}?"
	message="You can no longer log in with it, and its grant at the provider is revoked."
	confirmLabel="Unlink"
	danger
	onconfirm={unlink}
/>
<Confirm
	open={revokingToken !== null}
	title="Revoke {revokingToken?.name ?? 'this token'}?"
	message="Anything using it stops working."
	confirmLabel="Revoke"
	danger
	onconfirm={revokeToken}
/>
