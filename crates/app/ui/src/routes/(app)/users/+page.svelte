<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
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
	import Toolbar from '$lib/components/Toolbar.svelte';
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
	{#if user.id === session.user?.id}<span class="ml-2 text-sm text-surface-600-400">You</span>{/if}
{/snippet}
{#snippet roleCell(user: User)}
	<select
		class="select-sm select w-32"
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
			class="btn btn-sm hover:preset-tonal"
			onclick={() => {
				passwordFor = user;
				password = '';
			}}>Set password</button
		>
		<button
			type="button"
			class="btn btn-sm hover:preset-tonal-error"
			onclick={() => (deleting = user)}
			disabled={user.id === session.user?.id}>Delete</button
		>
	</span>
{/snippet}
{#snippet sessionUser(s: AccountSessionView)}
	<span class="font-medium">{s.username}</span>
	{#if s.current}<span class="ml-2 text-sm text-surface-600-400">This session</span>{/if}
{/snippet}
{#snippet sessionCreated(s: AccountSessionView)}<RelativeTime
		at={s.created_at}
		class="whitespace-nowrap"
	/>{/snippet}
{#snippet sessionSeen(s: AccountSessionView)}<RelativeTime
		at={s.last_seen_at}
		class="whitespace-nowrap"
	/>{/snippet}
{#snippet sessionExpires(s: AccountSessionView)}<RelativeTime
		at={s.expires_at}
		class="whitespace-nowrap"
	/>{/snippet}
{#snippet sessionActions(s: AccountSessionView)}
	<span class="flex justify-end gap-1">
		<button
			type="button"
			class="btn btn-sm hover:preset-tonal"
			onclick={() => (revokeAllFor = { id: s.user_id, username: s.username })}
			>End all of {s.username}</button
		>
		<button
			type="button"
			class="btn btn-sm hover:preset-tonal-error"
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
{#snippet tokenUsed(t: AccountTokenView)}<RelativeTime
		at={t.last_used_at}
		class="whitespace-nowrap"
	/>{/snippet}
{#snippet tokenExpires(t: AccountTokenView)}<RelativeTime
		at={t.expires_at}
		class="whitespace-nowrap"
	/>{/snippet}
{#snippet tokenActions(t: AccountTokenView)}
	<button
		type="button"
		class="btn btn-sm hover:preset-tonal-error"
		onclick={() => revokeToken(t)}
		disabled={revoking !== null}
	>
		{#if revoking === t.id}<Spinner />{/if}
		Revoke
	</button>
{/snippet}

<PageHeader title="Users" />

<Toolbar description="Accounts, their roles, and every live session and API token.">
	{#if tab === 'accounts'}
		<button type="button" class="btn preset-filled-primary-500" onclick={() => (createOpen = true)}>
			<PlusIcon class="size-4" />
			Create account
		</button>
	{/if}
</Toolbar>

<div
	class="flex gap-1 overflow-x-auto border-b border-surface-200-800"
	role="tablist"
	aria-label="Users"
>
	{#each TABS as entry (entry.id)}
		<button
			type="button"
			role="tab"
			aria-selected={tab === entry.id}
			class="border-b-2 px-3 py-2 text-sm font-medium whitespace-nowrap {tab === entry.id
				? 'border-primary-500'
				: 'border-transparent text-surface-600-400 hover:text-surface-950-50'}"
			onclick={() => (tab = entry.id)}
		>
			{entry.label}
			<span class="ml-2 text-surface-600-400">{number(entry.count())}</span>
		</button>
	{/each}
</div>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if tab === 'accounts'}
	<div role="tabpanel">
		<DataTable rows={users} columns={userColumns} rowKey={(u) => u.id} {loading} />
	</div>
{:else if tab === 'roles'}
	<div class="space-y-4" role="tabpanel">
		<p class="text-sm text-surface-600-400">
			Roles are fixed. Choose one per account on the <button
				type="button"
				class="link-body underline"
				onclick={() => (tab = 'accounts')}>Accounts tab</button
			>.
		</p>
		{#if loading && roles.length === 0}
			<ul
				class="divide-y divide-surface-200-800 rounded-container border border-surface-200-800 bg-surface-100-900"
				aria-busy="true"
			>
				{#each ROLES as role (role)}
					<li
						class="grid gap-4 p-5 sm:p-6 md:grid-cols-[16rem_minmax(0,1fr)_16rem] md:items-start md:gap-6"
						aria-hidden="true"
					>
						<div class="space-y-2">
							<div class="h-4 placeholder w-24 animate-pulse"></div>
							<div class="h-4 placeholder animate-pulse"></div>
						</div>
						<div class="h-6 placeholder animate-pulse"></div>
						<div class="h-4 placeholder w-40 animate-pulse"></div>
					</li>
				{/each}
			</ul>
		{:else}
			<ul
				class="divide-y divide-surface-200-800 rounded-container border border-surface-200-800 bg-surface-100-900"
			>
				{#each roles as role (role.role)}
					<li
						class="grid gap-4 p-5 sm:p-6 md:grid-cols-[16rem_minmax(0,1fr)_16rem] md:items-start md:gap-6"
					>
						<div class="space-y-1">
							<h2 class="h6">{ROLE_LABELS[role.role]}</h2>
							<p class="text-sm text-surface-600-400">{role.description}</p>
						</div>
						<p class="text-sm">
							{role.permissions.map((permission) => PERMISSION_LABELS[permission]).join(', ') ||
								'Read only'}
						</p>
						{#if role.accounts.length === 0}
							<p class="text-sm text-surface-600-400">No accounts</p>
						{:else}
							<p class="flex flex-wrap items-baseline gap-x-1 gap-y-0.5 text-sm">
								<span class="text-surface-600-400"
									>{number(role.accounts.length)} account{role.accounts.length === 1
										? ''
										: 's'}:</span
								>
								{#each role.accounts as account, index (account.id)}
									<span>
										<a
											class="link-body underline"
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
					</li>
				{/each}
			</ul>
		{/if}
	</div>
{:else if tab === 'sessions'}
	<div role="tabpanel">
		<DataTable rows={sessions} columns={sessionColumns} rowKey={(s) => s.id} {loading} dense />
	</div>
{:else}
	<div role="tabpanel">
		<DataTable rows={tokens} columns={tokenColumns} rowKey={(t) => t.id} {loading} dense>
			{#snippet empty()}
				No API tokens exist. Accounts create them under Account.
			{/snippet}
		</DataTable>
	</div>
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
