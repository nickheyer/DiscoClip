<script lang="ts">
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { Accordion, Tabs } from '@skeletonlabs/skeleton-svelte';
	import { onMount, tick } from 'svelte';
	import {
		roles as rolesApi,
		sessions as sessionsApi,
		tokens as tokensApi,
		users as usersApi
	} from '$lib/api/endpoints';
	import type { AccountSessionView, AccountTokenView, Role, RoleView, User } from '$lib/api/types';
	import { PERMISSION_LABELS } from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { EMPTY, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	type Tab = 'accounts' | 'roles' | 'sessions' | 'tokens';
	const ROLES: Role[] = ['admin', 'operator', 'viewer'];
	const ROLE_LABELS: Record<Role, string> = {
		admin: 'Admin',
		operator: 'Operator',
		viewer: 'Viewer'
	};

	let tab = $state<Tab>('accounts');
	let users = $state<User[]>([]);
	let roles = $state<RoleView[]>([]);
	let sessions = $state<AccountSessionView[]>([]);
	let tokens = $state<AccountTokenView[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	let createOpen = $state(false);
	let newUsername = $state('');
	let newPassword = $state('');
	let newRole = $state<Role>('viewer');
	let creating = $state(false);

	let passwordFor = $state<User | null>(null);
	let password = $state('');
	let settingPassword = $state(false);

	let deleting = $state<User | null>(null);
	let roleChanging = $state<string | null>(null);
	let revoking = $state<string | null>(null);
	let revokeAllFor = $state<{ id: string; username: string } | null>(null);

	async function load() {
		loading = true;
		error = null;
		try {
			[users, roles, sessions, tokens] = await Promise.all([
				usersApi.list(),
				rolesApi.list(),
				sessionsApi.listAll(),
				tokensApi.listAll()
			]);
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	/** Open the Accounts tab and bring one account's row into view. */
	async function showAccount(id: string) {
		tab = 'accounts';
		await tick();
		const row = document.getElementById(`user-${id}`);
		if (!row) return;
		row.scrollIntoView({ block: 'center' });
		row.focus();
	}

	function isTab(value: string): value is Tab {
		return value === 'accounts' || value === 'roles' || value === 'sessions' || value === 'tokens';
	}

	async function create(event: SubmitEvent) {
		event.preventDefault();
		creating = true;
		try {
			const user = await usersApi.create({
				username: newUsername.trim(),
				role: newRole,
				password: newPassword || undefined
			});
			notify.success('Account created', user.username);
			createOpen = false;
			newUsername = '';
			newPassword = '';
			newRole = 'viewer';
			await load();
		} catch (err) {
			reportError(err, 'Could not create the account');
		} finally {
			creating = false;
		}
	}

	async function changeRole(user: User, role: Role) {
		if (role === user.role) return;
		roleChanging = user.id;
		try {
			const updated = await usersApi.update(user.id, { role });
			users = users.map((u) => (u.id === user.id ? updated : u));
			notify.success('Role changed', `${updated.username} is now ${role}`);
			if (updated.id === session.user?.id) await session.refresh();
		} catch (err) {
			reportError(err, 'Could not change the role');
		} finally {
			roleChanging = null;
		}
	}

	async function setPassword(event: SubmitEvent) {
		event.preventDefault();
		if (!passwordFor) return;
		settingPassword = true;
		try {
			await usersApi.setPassword(passwordFor.id, { password });
			notify.success('Password set', `${passwordFor.username}'s other sessions ended`);
			passwordFor = null;
			password = '';
			await load();
		} catch (err) {
			reportError(err, 'Could not set the password');
		} finally {
			settingPassword = false;
		}
	}

	async function remove() {
		if (!deleting) return;
		await usersApi.remove(deleting.id);
		notify.success('Account deleted', deleting.username);
		deleting = null;
		await load();
	}

	async function revokeSession(s: AccountSessionView) {
		revoking = s.id;
		try {
			await usersApi.revokeSession(s.user_id, s.id);
			sessions = sessions.filter((x) => x.id !== s.id);
			if (s.current) await session.expire();
		} catch (err) {
			reportError(err, 'Could not end the session');
		} finally {
			revoking = null;
		}
	}

	async function revokeAll() {
		if (!revokeAllFor) return;
		const target = revokeAllFor;
		const result = await usersApi.revokeSessions(target.id);
		notify.success(`${number(result.revoked)} sessions ended`, target.username);
		revokeAllFor = null;
		if (target.id === session.user?.id) await session.expire();
		else sessions = sessions.filter((s) => s.user_id !== target.id);
	}

	async function revokeToken(t: AccountTokenView) {
		revoking = t.id;
		try {
			await usersApi.revokeToken(t.user_id, t.id);
			tokens = tokens.filter((x) => x.id !== t.id);
			notify.success('Token revoked', t.name);
		} catch (err) {
			reportError(err, 'Could not revoke the token');
		} finally {
			revoking = null;
		}
	}

	const userColumns: Column<User>[] = [
		{
			key: 'username',
			label: 'Username',
			cell: usernameCell,
			sortable: true,
			value: (u) => u.username
		},
		{ key: 'role', label: 'Role', cell: roleCell, sortable: true, value: (u) => u.role },
		{
			key: 'password',
			label: 'Password',
			value: (u) => (u.has_password ? 'Set' : `${EMPTY}, provider login only`)
		},
		{
			key: 'created',
			label: 'Created',
			cell: createdCell,
			sortable: true,
			value: (u) => u.created_at
		},
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: userActions, align: 'right' }
	];
	const sessionColumns: Column<AccountSessionView>[] = [
		{
			key: 'username',
			label: 'Account',
			cell: sessionUser,
			sortable: true,
			value: (s) => s.username
		},
		{ key: 'ip', label: 'Address', value: (s) => s.ip, class: 'font-mono text-xs' },
		{
			key: 'agent',
			label: 'Browser',
			value: (s) => s.user_agent,
			class: 'max-w-56 truncate text-sm'
		},
		{ key: 'created', label: 'Started', cell: sessionCreated },
		{ key: 'seen', label: 'Last seen', cell: sessionSeen },
		{ key: 'expires', label: 'Expires', cell: sessionExpires },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: sessionActions, align: 'right' }
	];
	const tokenColumns: Column<AccountTokenView>[] = [
		{ key: 'username', label: 'Account', value: (t) => t.username, sortable: true },
		{ key: 'name', label: 'Token', cell: tokenName, sortable: true, value: (t) => t.name },
		{ key: 'scopes', label: 'Scopes', cell: tokenScopes },
		{ key: 'used', label: 'Last used', cell: tokenUsed },
		{ key: 'expires', label: 'Expires', cell: tokenExpires },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: tokenActions, align: 'right' }
	];

	const TABS: { id: Tab; label: string; count: () => number }[] = [
		{ id: 'accounts', label: 'Accounts', count: () => users.length },
		{ id: 'roles', label: 'Roles', count: () => roles.length },
		{ id: 'sessions', label: 'Sessions', count: () => sessions.length },
		{ id: 'tokens', label: 'API tokens', count: () => tokens.length }
	];
</script>

{#snippet usernameCell(user: User)}
	<span id="user-{user.id}" tabindex="-1" class="font-medium">{user.username}</span>
	{#if user.id === session.user?.id}
		<span class="ml-2 badge preset-tonal" style="--badge-size: var(--text-xs)">You</span>
	{/if}
{/snippet}
{#snippet roleCell(user: User)}
	<select
		class="select w-32"
		value={user.role}
		onchange={(event) => changeRole(user, event.currentTarget.value as Role)}
		disabled={roleChanging !== null}
		aria-label="Role of {user.username}"
	>
		{#each ROLES as role (role)}<option value={role}>{ROLE_LABELS[role]}</option>{/each}
	</select>
{/snippet}
{#snippet createdCell(user: User)}
	<RelativeTime at={user.created_at} class="whitespace-nowrap" />
{/snippet}
{#snippet userActions(user: User)}
	<span class="flex justify-end gap-1">
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={() => {
				passwordFor = user;
				password = '';
			}}>Set password</button
		>
		<button
			type="button"
			class="btn preset-tonal-error btn-sm"
			onclick={() => (deleting = user)}
			disabled={user.id === session.user?.id}>Delete</button
		>
	</span>
{/snippet}
{#snippet sessionUser(s: AccountSessionView)}
	<span class="font-medium">{s.username}</span>
	{#if s.current}
		<span class="ml-2 badge preset-tonal" style="--badge-size: var(--text-xs)">This session</span>
	{/if}
{/snippet}
{#snippet sessionCreated(s: AccountSessionView)}
	<RelativeTime at={s.created_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionSeen(s: AccountSessionView)}
	<RelativeTime at={s.last_seen_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionExpires(s: AccountSessionView)}
	<RelativeTime at={s.expires_at} class="whitespace-nowrap" />
{/snippet}
{#snippet sessionActions(s: AccountSessionView)}
	<span class="flex justify-end gap-1">
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={() => (revokeAllFor = { id: s.user_id, username: s.username })}
			>End all of {s.username}</button
		>
		<button
			type="button"
			class="btn preset-tonal-error btn-sm"
			onclick={() => revokeSession(s)}
			disabled={revoking !== null}
		>
			{#if revoking === s.id}<Spinner />{/if}
			End
		</button>
	</span>
{/snippet}
{#snippet tokenName(t: AccountTokenView)}
	<span class="font-medium">{t.name}</span>
	<span class="ml-2 font-mono text-xs text-surface-600-400">{t.prefix}…</span>
{/snippet}
{#snippet tokenScopes(t: AccountTokenView)}
	{t.scopes.map((scope) => PERMISSION_LABELS[scope]).join(', ') || 'Read only'}
{/snippet}
{#snippet tokenUsed(t: AccountTokenView)}
	<RelativeTime at={t.last_used_at} class="whitespace-nowrap" />
{/snippet}
{#snippet tokenExpires(t: AccountTokenView)}
	<RelativeTime at={t.expires_at} class="whitespace-nowrap" />
{/snippet}
{#snippet tokenActions(t: AccountTokenView)}
	<button
		type="button"
		class="btn preset-tonal-error btn-sm"
		onclick={() => revokeToken(t)}
		disabled={revoking !== null}
	>
		{#if revoking === t.id}<Spinner />{/if}
		Revoke
	</button>
{/snippet}

<PageHeader
	title="Users"
	description="Accounts, their roles, and every live session and API token."
>
	{#snippet actions()}
		{#if tab === 'accounts'}
			<button
				type="button"
				class="btn preset-filled-primary-500"
				onclick={() => (createOpen = true)}
			>
				<PlusIcon class="size-4" />
				Create account
			</button>
		{/if}
	{/snippet}
</PageHeader>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<!-- Skeleton's Tabs: one per kind of record, each with its count. -->
	<Tabs
		value={tab}
		onValueChange={(details) => {
			if (isTab(details.value)) tab = details.value;
		}}
	>
		<Tabs.List class="overflow-x-auto">
			{#each TABS as entry (entry.id)}
				<Tabs.Trigger value={entry.id} class="gap-2">
					{entry.label}
					<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
						{number(entry.count())}
					</span>
				</Tabs.Trigger>
			{/each}
			<Tabs.Indicator />
		</Tabs.List>
		<Tabs.Content value="accounts">
			<DataTable rows={users} columns={userColumns} rowKey={(u) => u.id} {loading} />
		</Tabs.Content>
		<Tabs.Content value="roles" class="space-y-4">
			<p class="text-sm text-surface-600-400">
				Roles are fixed. Choose one per account on the
				<button type="button" class="anchor" onclick={() => (tab = 'accounts')}>Accounts tab</button
				>.
			</p>
			{#if loading && roles.length === 0}
				<div class="space-y-3 card preset-filled-surface-100-900 p-4" aria-busy="true">
					{#each ROLES as role (role)}
						<div class="h-10 placeholder animate-pulse" aria-hidden="true"></div>
					{/each}
				</div>
			{:else}
				<!-- Skeleton's Accordion: every role open, each folding to its name and count. -->
				<Accordion
					multiple
					collapsible
					defaultValue={roles.map((role) => role.role)}
					class="card preset-filled-surface-100-900 p-2"
				>
					{#each roles as role, i (role.role)}
						{#if i !== 0}
							<hr class="hr" />
						{/if}
						<Accordion.Item value={role.role}>
							<h3>
								<Accordion.ItemTrigger class="flex items-center justify-between gap-2">
									<span class="flex flex-wrap items-center gap-2">
										<span class="font-semibold">{ROLE_LABELS[role.role]}</span>
										<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
											{number(role.accounts.length)}
											{role.accounts.length === 1 ? 'account' : 'accounts'}
										</span>
									</span>
									<Accordion.ItemIndicator class="group">
										<ChevronDownIcon class="size-4 transition group-data-[state=open]:rotate-180" />
									</Accordion.ItemIndicator>
								</Accordion.ItemTrigger>
							</h3>
							<Accordion.ItemContent class="space-y-2 text-sm">
								<p class="text-surface-600-400">{role.description}</p>
								<p>
									{role.permissions.map((permission) => PERMISSION_LABELS[permission]).join(', ') ||
										'Read only'}
								</p>
								{#if role.accounts.length === 0}
									<p class="text-surface-600-400">No accounts</p>
								{:else}
									<p class="flex flex-wrap items-baseline gap-x-1 gap-y-0.5">
										{#each role.accounts as account, index (account.id)}
											<span>
												<a
													class="anchor"
													href="#user-{account.id}"
													onclick={(event) => {
														event.preventDefault();
														void showAccount(account.id);
													}}>{account.username}</a
												>{#if index < role.accounts.length - 1},{/if}
											</span>
										{/each}
									</p>
								{/if}
							</Accordion.ItemContent>
						</Accordion.Item>
					{/each}
				</Accordion>
			{/if}
		</Tabs.Content>
		<Tabs.Content value="sessions">
			<DataTable rows={sessions} columns={sessionColumns} rowKey={(s) => s.id} {loading} />
		</Tabs.Content>
		<Tabs.Content value="tokens">
			<DataTable rows={tokens} columns={tokenColumns} rowKey={(t) => t.id} {loading}>
				{#snippet empty()}
					No API tokens exist. Accounts create them under Account.
				{/snippet}
			</DataTable>
		</Tabs.Content>
	</Tabs>
{/if}

<Modal bind:open={createOpen} title="Create an account" busy={creating}>
	<form id="create-user" class="space-y-4" onsubmit={create}>
		<Field label="Username" for="new-username" required>
			<input
				id="new-username"
				class="input"
				type="text"
				bind:value={newUsername}
				required
				autocomplete="off"
			/>
		</Field>
		<Field label="Role" for="new-role" required>
			<select id="new-role" class="select" bind:value={newRole}>
				{#each ROLES as role (role)}<option value={role}>{ROLE_LABELS[role]}</option>{/each}
			</select>
		</Field>
		<Field
			label="Password"
			for="new-password"
			help="Leave empty for an account that logs in through a provider only."
		>
			<input
				id="new-password"
				class="input"
				type="password"
				bind:value={newPassword}
				autocomplete="new-password"
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (createOpen = false)}
			disabled={creating}>Cancel</button
		>
		<button
			type="submit"
			form="create-user"
			class="btn preset-filled-primary-500"
			disabled={creating}
		>
			{#if creating}<Spinner />{/if}
			Create
		</button>
	{/snippet}
</Modal>

<Modal
	open={passwordFor !== null}
	title="Set the password of {passwordFor?.username ?? ''}"
	description="Their other sessions end."
	busy={settingPassword}
>
	<form id="set-password" class="space-y-4" onsubmit={setPassword}>
		<Field label="New password" for="set-password-value" required>
			<input
				id="set-password-value"
				class="input"
				type="password"
				bind:value={password}
				required
				autocomplete="new-password"
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (passwordFor = null)}
			disabled={settingPassword}>Cancel</button
		>
		<button
			type="submit"
			form="set-password"
			class="btn preset-filled-primary-500"
			disabled={settingPassword}
		>
			{#if settingPassword}<Spinner />{/if}
			Set password
		</button>
	{/snippet}
</Modal>

<Confirm
	open={deleting !== null}
	title="Delete {deleting?.username ?? 'this account'}?"
	message="Its sessions, tokens and provider links go with it."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
<Confirm
	open={revokeAllFor !== null}
	title="End every session of {revokeAllFor?.username ?? ''}?"
	message="They are logged out everywhere."
	confirmLabel="End all"
	danger
	onconfirm={revokeAll}
/>
