<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { ROLES, messageOf, users } from '$lib/api';
	import type { Role } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { describeUserAgent, pluralize } from '$lib/format';
	import { PERMISSION_LABELS, ROLE_LABELS } from '$lib/permissions';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { matchProblem, passwordProblem } from '$lib/validation';

	let { data }: { data: PageData } = $props();

	const user = $derived(data.user);
	const isMe = $derived(user.id === session.user?.id);
	const refresh = () => invalidate(`app:user:${user.id}`);

	// Role

	let role = $state<Role>('viewer');
	$effect.pre(() => {
		role = user.role;
	});
	let savingRole = $state(false);
	let roleError = $state<string | null>(null);

	async function saveRole(event: SubmitEvent) {
		event.preventDefault();
		savingRole = true;
		roleError = null;
		try {
			const updated = await users.update(user.id, { role });
			toast.ok(`${updated.username} is now ${ROLE_LABELS[updated.role].label.toLowerCase()}.`);
			await refresh();
			if (isMe) await session.load();
		} catch (cause) {
			roleError = messageOf(cause);
		} finally {
			savingRole = false;
		}
	}

	// Password

	let password = $state('');
	let confirmation = $state('');
	let savingPassword = $state(false);
	let passwordError = $state<string | null>(null);
	const newPasswordProblem = $derived(passwordProblem(password));
	const confirmProblem = $derived(matchProblem(password, confirmation));
	const passwordReady = $derived(password !== '' && confirmation === password && !newPasswordProblem);

	async function resetPassword(event: SubmitEvent) {
		event.preventDefault();
		if (!passwordReady) return;
		savingPassword = true;
		passwordError = null;
		try {
			await users.setPassword(user.id, { password });
			password = '';
			confirmation = '';
			toast.ok(`Password set for ${user.username}. Their sessions were ended.`);
			if (isMe) {
				await session.leave();
				return;
			}
			await refresh();
		} catch (cause) {
			passwordError = messageOf(cause);
		} finally {
			savingPassword = false;
		}
	}

	// Sessions

	let endingAll = $state(false);

	async function endAllSessions() {
		const ok = await confirm.ask({
			title: `End every session of ${user.username}?`,
			message: isMe
				? 'That includes the one you are using now, so you will be logged out.'
				: `${pluralize(data.sessions.length, 'session')} will be logged out.`,
			confirmLabel: 'End sessions',
			danger: true
		});
		if (!ok) return;
		endingAll = true;
		try {
			const { revoked } = await users.revokeSessions(user.id);
			if (isMe) {
				await session.leave();
				return;
			}
			toast.ok(`Ended ${pluralize(revoked, 'session')}.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not end the sessions: ${messageOf(cause)}`);
		} finally {
			endingAll = false;
		}
	}

	// Tokens

	let revoking = $state<string | null>(null);

	async function revokeToken(id: string, name: string) {
		const ok = await confirm.ask({
			title: `Revoke “${name}”?`,
			message: 'Anything still using this token stops working at once.',
			confirmLabel: 'Revoke',
			danger: true
		});
		if (!ok) return;
		revoking = id;
		try {
			await users.revokeToken(user.id, id);
			toast.ok(`Revoked ${name}.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not revoke the token: ${messageOf(cause)}`);
		} finally {
			revoking = null;
		}
	}

	// Delete

	let deleting = $state(false);

	async function remove() {
		const ok = await confirm.ask({
			title: `Delete ${user.username}?`,
			message: 'The account goes away with its sessions, API tokens and linked logins. This cannot be undone.',
			confirmLabel: 'Delete account',
			danger: true,
			typed: user.username
		});
		if (!ok) return;
		deleting = true;
		try {
			await users.remove(user.id);
			toast.ok(`Deleted ${user.username}.`);
			await invalidate('app:users');
			await goto('/users');
		} catch (cause) {
			toast.error(`Could not delete the account: ${messageOf(cause)}`);
			deleting = false;
		}
	}
</script>

<svelte:head>
	<title>{user.username} · Accounts · DiscoClip</title>
</svelte:head>

<PageHeader title={user.username} crumbs={[{ label: 'Accounts', href: '/users' }, { label: user.username }]}>
	{#snippet meta()}
		<Badge tone={user.role === 'admin' ? 'accent' : 'neutral'}>{ROLE_LABELS[user.role].label}</Badge>
		{#if isMe}<Badge tone="info">This is you</Badge>{/if}
		{#if !user.has_password}<Badge tone="warn">No password</Badge>{/if}
		<span class="faint small">Created <Time value={user.created_at} /> · updated <Time value={user.updated_at} /></span>
	{/snippet}
	{#snippet actions()}
		{#if isMe}
			<Button href="/account" icon="user">Your account page</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<div class="grid-2 top">
		<section class="card">
			<div class="card-header"><h2>Role</h2></div>
			<form class="card-body stack" onsubmit={saveRole}>
				{#if roleError}
					<Alert tone="danger" message={roleError} onclose={() => (roleError = null)} />
				{/if}
				{#each ROLES as option (option)}
					<label class="radio">
						<input type="radio" name="role" value={option} bind:group={role} />
						<span>
							<span class="strong">{ROLE_LABELS[option].label}</span>
							<span class="hint">{ROLE_LABELS[option].description}</span>
						</span>
					</label>
				{/each}
				<div class="row-between">
					<p class="hint">The last admin cannot be demoted.</p>
					<Button type="submit" variant="primary" loading={savingRole} disabled={role === user.role}>Save role</Button>
				</div>
			</form>
		</section>

		<section class="card">
			<div class="card-header"><h2>{user.has_password ? 'Reset password' : 'Set a password'}</h2></div>
			<form class="card-body stack" onsubmit={resetPassword} novalidate>
				{#if passwordError}
					<Alert tone="danger" message={passwordError} onclose={() => (passwordError = null)} />
				{/if}
				<Field label="New password" for="reset-password" error={newPasswordProblem} hint="8 to 256 characters.">
					<PasswordInput id="reset-password" bind:value={password} autocomplete="new-password" invalid={!!newPasswordProblem} required />
				</Field>
				<Field label="Confirm" for="reset-confirm" error={confirmProblem}>
					<PasswordInput id="reset-confirm" bind:value={confirmation} autocomplete="new-password" invalid={!!confirmProblem} required />
				</Field>
				<div class="row-between">
					<p class="hint">{isMe ? 'Every session of yours ends, this one included.' : 'Every session of the account ends.'}</p>
					<Button type="submit" variant="primary" loading={savingPassword} disabled={!passwordReady}>Set password</Button>
				</div>
			</form>
		</section>
	</div>

	<section class="card">
		<div class="card-header">
			<h2>Sessions</h2>
			<Button size="sm" variant="danger-soft" icon="logout" loading={endingAll} disabled={data.sessions.length === 0} onclick={endAllSessions}>End all sessions</Button>
		</div>
		{#if data.sessions.length === 0}
			<div class="card-body"><p class="muted">Not logged in anywhere.</p></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr><th>Client</th><th>Address</th><th>Started</th><th>Last seen</th><th>Expires</th></tr>
					</thead>
					<tbody>
						{#each data.sessions as view (view.id)}
							<tr>
								<td>
									<div class="row">
										<Icon name={/Android|iPhone|iPad|Mobile/.test(view.user_agent ?? '') ? 'phone' : 'laptop'} size={15} />
										<span title={view.user_agent ?? undefined}>{describeUserAgent(view.user_agent)}</span>
										{#if view.current}<Badge tone="ok" size="sm">Your current session</Badge>{/if}
									</div>
								</td>
								<td class="mono">{view.ip ?? '—'}</td>
								<td><Time value={view.created_at} /></td>
								<td><Time value={view.last_seen_at} /></td>
								<td><Time value={view.expires_at} /></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="card">
		<div class="card-header"><h2>API tokens</h2></div>
		{#if data.tokens.length === 0}
			<div class="card-body"><Empty compact icon="key" title="No API tokens" description="Accounts mint their own tokens from their account page." /></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr><th>Name</th><th>Prefix</th><th>Scopes</th><th>Created</th><th>Last used</th><th>Expires</th><th></th></tr>
					</thead>
					<tbody>
						{#each data.tokens as token (token.id)}
							<tr>
								<td class="strong">{token.name}</td>
								<td><code>{token.prefix}…</code></td>
								<td>
									{#if token.scopes.length === 0}<span class="faint">none</span>{:else}
										<div class="chips">{#each token.scopes as scope (scope)}<Badge size="sm">{PERMISSION_LABELS[scope].label}</Badge>{/each}</div>
									{/if}
								</td>
								<td><Time value={token.created_at} /></td>
								<td><Time value={token.last_used_at} empty="never" /></td>
								<td><Time value={token.expires_at} empty="never" /></td>
								<td class="actions"><Button size="sm" variant="ghost" loading={revoking === token.id} onclick={() => revokeToken(token.id, token.name)}>Revoke</Button></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="card card-danger">
		<div class="card-header"><h2>Delete account</h2></div>
		<div class="card-body row-between">
			<div class="row">
				<Avatar name={user.username} size={32} />
				<p class="muted">Removes {user.username} with every session, API token and linked login. The last admin cannot be removed.</p>
			</div>
			<Button variant="danger" icon="trash" loading={deleting} onclick={remove}>Delete account</Button>
		</div>
	</section>
</div>

<style>
	.top {
		align-items: start;
	}

	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}
</style>
