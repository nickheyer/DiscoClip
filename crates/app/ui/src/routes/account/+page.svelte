<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { page } from '$app/state';
	import type { PageData } from './$types';
	import { auth, isApiError, messageOf, providers as providerApi, tokens, users } from '$lib/api';
	import type { Identity, Minted, Permission, ProviderInfo, SessionView } from '$lib/api';
	import { callbackErrorMessage, providerIcon } from '$lib/auth-messages';
	import Alert from '$lib/components/Alert.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { describeUserAgent, pluralize } from '$lib/format';
	import { PERMISSION_LABELS, ROLE_LABELS, ROLE_PERMISSIONS } from '$lib/permissions';
	import { clock } from '$lib/state/clock.svelte';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { PERMISSIONS } from '$lib/api';
	import { TOKEN_NAME_MAX, matchProblem, passwordProblem } from '$lib/validation';

	let { data }: { data: PageData } = $props();

	const me = $derived(session.me!);
	const user = $derived(me.user);
	const granted = $derived(ROLE_PERMISSIONS[user.role]);
	const flowError = $derived(callbackErrorMessage(page.url.searchParams.get('error')));

	const refresh = () => invalidate('app:account');

	// Password

	let currentPassword = $state('');
	let newPassword = $state('');
	let confirmation = $state('');
	let savingPassword = $state(false);
	let passwordError = $state<string | null>(null);
	let passwordRetryAt = $state<number | null>(null);

	const passwordWait = $derived(
		passwordRetryAt ? Math.max(0, Math.ceil((passwordRetryAt - clock.now) / 1000)) : 0
	);
	const newPasswordProblem = $derived(passwordProblem(newPassword));
	const confirmProblem = $derived(matchProblem(newPassword, confirmation));
	const passwordReady = $derived(
		currentPassword !== '' &&
			newPassword !== '' &&
			confirmation === newPassword &&
			!newPasswordProblem &&
			passwordWait === 0
	);

	async function changePassword(event: SubmitEvent) {
		event.preventDefault();
		if (!passwordReady) return;
		savingPassword = true;
		passwordError = null;
		try {
			await users.setPassword(user.id, { password: newPassword, current_password: currentPassword });
			currentPassword = '';
			newPassword = '';
			confirmation = '';
			toast.ok('Password changed. Your other sessions were ended.');
			await refresh();
		} catch (cause) {
			if (isApiError(cause) && cause.status === 429) {
				passwordRetryAt = Date.now() + (cause.retryAfter ?? 60) * 1000;
			}
			passwordError = messageOf(cause);
		} finally {
			savingPassword = false;
		}
	}

	// Sessions

	let endingSession = $state<string | null>(null);
	let endingOthers = $state(false);
	const otherSessions = $derived(data.sessions.filter((s) => !s.current));

	async function endSession(view: SessionView) {
		if (view.current) {
			const ok = await confirm.ask({
				title: 'End this session?',
				message: 'This is the session you are using now. Ending it logs you out here.',
				confirmLabel: 'Log out',
				danger: true
			});
			if (!ok) return;
		}
		endingSession = view.id;
		try {
			await auth.revokeSession(view.id);
			if (view.current) {
				await session.leave();
				return;
			}
			toast.ok('Session ended.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not end the session: ${messageOf(cause)}`);
		} finally {
			endingSession = null;
		}
	}

	async function endOthers() {
		const ok = await confirm.ask({
			title: 'End your other sessions?',
			message: `${pluralize(otherSessions.length, 'other session')} will be logged out. This one stays.`,
			confirmLabel: 'End them',
			danger: true
		});
		if (!ok) return;
		endingOthers = true;
		try {
			const { revoked } = await auth.revokeOtherSessions();
			toast.ok(`Ended ${pluralize(revoked, 'session')}.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not end the sessions: ${messageOf(cause)}`);
		} finally {
			endingOthers = false;
		}
	}

	// API tokens

	let tokenDialog = $state(false);
	let tokenName = $state('');
	let tokenScopes = $state<Permission[]>([]);
	let tokenExpiry = $state<'never' | '7' | '30' | '90' | '365' | 'custom'>('90');
	let tokenCustomDays = $state('');
	let mintingToken = $state(false);
	let tokenError = $state<string | null>(null);
	let minted = $state<Minted | null>(null);
	let revokingToken = $state<string | null>(null);

	const customDaysProblem = $derived(
		tokenExpiry === 'custom' &&
			tokenCustomDays !== '' &&
			!(Number.isInteger(Number(tokenCustomDays)) && Number(tokenCustomDays) > 0)
			? 'A number of days, at least 1.'
			: null
	);
	const expiresInDays = $derived.by((): number | undefined => {
		if (tokenExpiry === 'never') return undefined;
		if (tokenExpiry === 'custom') return tokenCustomDays === '' ? undefined : Number(tokenCustomDays);
		return Number(tokenExpiry);
	});
	const tokenReady = $derived(
		tokenName.trim() !== '' &&
			tokenName.trim().length <= TOKEN_NAME_MAX &&
			!customDaysProblem &&
			(tokenExpiry !== 'custom' || tokenCustomDays !== '')
	);

	function openTokenDialog() {
		tokenName = '';
		tokenScopes = [];
		tokenExpiry = '90';
		tokenCustomDays = '';
		tokenError = null;
		tokenDialog = true;
	}

	function toggleScope(scope: Permission, on: boolean) {
		tokenScopes = on ? [...new Set([...tokenScopes, scope])] : tokenScopes.filter((s) => s !== scope);
	}

	async function mintToken(event: SubmitEvent) {
		event.preventDefault();
		if (!tokenReady) return;
		mintingToken = true;
		tokenError = null;
		try {
			minted = await tokens.create({
				name: tokenName.trim(),
				scopes: tokenScopes,
				expires_in_days: expiresInDays
			});
			tokenDialog = false;
			await refresh();
		} catch (cause) {
			tokenError = messageOf(cause);
		} finally {
			mintingToken = false;
		}
	}

	async function revokeToken(id: string, name: string) {
		const ok = await confirm.ask({
			title: `Revoke “${name}”?`,
			message: 'Anything still using this token stops working at once.',
			confirmLabel: 'Revoke',
			danger: true
		});
		if (!ok) return;
		revokingToken = id;
		try {
			await tokens.revoke(id);
			toast.ok(`Revoked ${name}.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not revoke the token: ${messageOf(cause)}`);
		} finally {
			revokingToken = null;
		}
	}

	// Linked logins

	let refreshingIdentity = $state<string | null>(null);
	let unlinkingIdentity = $state<string | null>(null);

	interface ProviderRow {
		id: string;
		name: string;
		offered: boolean;
		identity: Identity | null;
	}

	const providerRows = $derived.by((): ProviderRow[] => {
		const rows: ProviderRow[] = data.providers.map((p: ProviderInfo) => ({
			id: p.id,
			name: p.name,
			offered: true,
			identity: data.identities.find((i) => i.provider === p.id) ?? null
		}));
		for (const identity of data.identities) {
			if (!rows.some((r) => r.id === identity.provider)) {
				rows.push({ id: identity.provider, name: identity.provider, offered: false, identity });
			}
		}
		return rows;
	});

	function identityLabel(identity: Identity): string {
		return identity.display_name ?? identity.username ?? identity.email ?? identity.subject;
	}

	async function refreshIdentity(provider: string) {
		refreshingIdentity = provider;
		try {
			await providerApi.refresh(provider);
			toast.ok('Profile fetched again and tokens renewed.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not refresh: ${messageOf(cause)}`);
		} finally {
			refreshingIdentity = null;
		}
	}

	async function unlink(row: ProviderRow) {
		const ok = await confirm.ask({
			title: `Unlink ${row.name}?`,
			message: 'The grant is revoked at the provider too, where the provider allows it.',
			confirmLabel: 'Unlink',
			danger: true
		});
		if (!ok) return;
		unlinkingIdentity = row.id;
		try {
			const { revoked } = await providerApi.unlink(row.id);
			toast.ok(
				revoked
					? `Unlinked ${row.name} and revoked its grant.`
					: `Unlinked ${row.name}. The provider did not confirm revoking the grant.`
			);
			await refresh();
		} catch (cause) {
			toast.error(`Could not unlink: ${messageOf(cause)}`);
		} finally {
			unlinkingIdentity = null;
		}
	}
</script>

<svelte:head>
	<title>Account · DiscoClip</title>
</svelte:head>

<PageHeader title="Account" description="Your password, sessions, API tokens and linked logins." />

<div class="stack-lg">
	{#if flowError}
		<Alert tone="danger" title="Linking failed" message={flowError} />
	{/if}

	<section class="card">
		<div class="card-body profile">
			<Avatar name={user.username} size={56} />
			<div class="stack-sm profile-text">
				<div class="row">
					<h2 class="username">{user.username}</h2>
					<Badge tone="accent">{ROLE_LABELS[user.role].label}</Badge>
					{#if !user.has_password}<Badge tone="warn">No password</Badge>{/if}
				</div>
				<p class="muted">{ROLE_LABELS[user.role].description}</p>
				<div class="chips">
					{#each PERMISSIONS as permission (permission)}
						<span
							class={['perm', granted.includes(permission) ? 'on' : 'off']}
							title={PERMISSION_LABELS[permission].description}
						>
							<Icon name={granted.includes(permission) ? 'check' : 'x'} size={12} />
							{PERMISSION_LABELS[permission].label}
						</span>
					{/each}
				</div>
				<p class="faint small">
					Member since <Time value={user.created_at} mode="absolute" /> · id <code>{user.id}</code>
				</p>
			</div>
		</div>
	</section>

	<section class="card" id="password">
		<div class="card-header"><h2>Password</h2></div>
		<div class="card-body">
			{#if user.has_password}
				<form class="stack narrow" onsubmit={changePassword} novalidate>
					{#if passwordError}
						<Alert tone="danger" message={passwordError} onclose={() => (passwordError = null)} />
					{/if}
					{#if passwordWait > 0}
						<Alert tone="warn" message={`Too many wrong passwords. Try again in ${passwordWait}s.`} />
					{/if}
					<Field label="Current password" for="pw-current">
						<PasswordInput id="pw-current" bind:value={currentPassword} autocomplete="current-password" required />
					</Field>
					<Field label="New password" for="pw-new" error={newPasswordProblem} hint="8 to 256 characters.">
						<PasswordInput id="pw-new" bind:value={newPassword} autocomplete="new-password" invalid={!!newPasswordProblem} required />
					</Field>
					<Field label="Confirm new password" for="pw-confirm" error={confirmProblem}>
						<PasswordInput id="pw-confirm" bind:value={confirmation} autocomplete="new-password" invalid={!!confirmProblem} required />
					</Field>
					<div class="row-between">
						<p class="hint">Changing it ends every other session of yours.</p>
						<Button type="submit" variant="primary" loading={savingPassword} disabled={!passwordReady}>Change password</Button>
					</div>
				</form>
			{:else}
				<Alert
					tone="info"
					title="This account has no password"
					message="You log in through a linked provider. An admin can set a password for you from the accounts page."
				/>
			{/if}
		</div>
	</section>

	<section class="card" id="sessions">
		<div class="card-header">
			<h2>Sessions</h2>
			<Button size="sm" variant="danger-soft" icon="logout" loading={endingOthers} disabled={otherSessions.length === 0} onclick={endOthers}>
				End all other sessions
			</Button>
		</div>
		<div class="table-wrap flush">
			<table class="table">
				<thead>
					<tr>
						<th>Client</th>
						<th>Address</th>
						<th>Started</th>
						<th>Last seen</th>
						<th>Expires</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each data.sessions as view (view.id)}
						<tr>
							<td>
								<div class="row">
									<Icon name={/Android|iPhone|iPad|Mobile/.test(view.user_agent ?? '') ? 'phone' : 'laptop'} size={15} />
									<span title={view.user_agent ?? undefined}>{describeUserAgent(view.user_agent)}</span>
									{#if view.current}<Badge tone="ok" size="sm">This device</Badge>{/if}
								</div>
							</td>
							<td class="mono">{view.ip ?? '—'}</td>
							<td><Time value={view.created_at} /></td>
							<td><Time value={view.last_seen_at} /></td>
							<td><Time value={view.expires_at} /></td>
							<td class="actions">
								<Button size="sm" variant="ghost" loading={endingSession === view.id} onclick={() => endSession(view)}>
									{view.current ? 'Log out' : 'End'}
								</Button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>

	<section class="card" id="tokens">
		<div class="card-header">
			<div>
				<h2>API tokens</h2>
				<p class="hint">Sent as <code>Authorization: Bearer dc_…</code>. Each does only what it was given.</p>
			</div>
			<Button size="sm" variant="primary" icon="plus" onclick={openTokenDialog}>New token</Button>
		</div>
		{#if data.tokens.length === 0}
			<div class="card-body">
				<Empty compact icon="key" title="No API tokens" description="Mint one for scripts and integrations; the secret is shown once." />
			</div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Name</th>
							<th>Prefix</th>
							<th>Scopes</th>
							<th>Created</th>
							<th>Last used</th>
							<th>Expires</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each data.tokens as token (token.id)}
							<tr>
								<td class="strong">{token.name}</td>
								<td><code>{token.prefix}…</code></td>
								<td>
									{#if token.scopes.length === 0}
										<span class="faint">none</span>
									{:else}
										<div class="chips">
											{#each token.scopes as scope (scope)}
												<Badge size="sm">{PERMISSION_LABELS[scope].label}</Badge>
											{/each}
										</div>
									{/if}
								</td>
								<td><Time value={token.created_at} /></td>
								<td><Time value={token.last_used_at} empty="never" /></td>
								<td><Time value={token.expires_at} empty="never" /></td>
								<td class="actions">
									<Button size="sm" variant="ghost" loading={revokingToken === token.id} onclick={() => revokeToken(token.id, token.name)}>Revoke</Button>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="card" id="logins">
		<div class="card-header">
			<div>
				<h2>Linked logins</h2>
				<p class="hint">Providers you can log in with instead of a password.</p>
			</div>
		</div>
		{#if providerRows.length === 0}
			<div class="card-body">
				<Empty compact icon="link" title="No login providers" description="This server offers no provider logins. An admin configures GitHub, Google, an OpenID Connect issuer, or marks a Discord application for login." />
			</div>
		{:else}
			<ul class="providers">
				{#each providerRows as row (row.id)}
					<li class="provider">
						<span class="provider-icon"><Icon name={providerIcon(row.id)} size={20} /></span>
						<div class="provider-text">
							<div class="row">
								<span class="strong">{row.name}</span>
								{#if row.identity}
									<Badge tone="ok" size="sm">Linked</Badge>
								{:else}
									<Badge size="sm">Not linked</Badge>
								{/if}
								{#if !row.offered}<Badge tone="warn" size="sm">No longer offered</Badge>{/if}
							</div>
							{#if row.identity}
								<dl class="kv small">
									<dt>Identity</dt>
									<dd>
										{identityLabel(row.identity)}
										{#if row.identity.email && row.identity.email !== identityLabel(row.identity)}
											<span class="faint">· {row.identity.email}</span>
										{/if}
										<span class="faint">· <code>{row.identity.subject}</code></span>
									</dd>
									<dt>Granted</dt>
									<dd>
										{row.identity.scope ?? '—'}
										{#if row.identity.has_refresh_token}<span class="faint">· renewable</span>{/if}
									</dd>
									<dt>Token expires</dt>
									<dd><Time value={row.identity.expires_at} empty="not reported" /></dd>
									<dt>Linked</dt>
									<dd><Time value={row.identity.linked_at} /> <span class="faint">· updated <Time value={row.identity.updated_at} /></span></dd>
								</dl>
							{/if}
						</div>
						<div class="provider-actions">
							{#if row.identity}
								{#if row.offered}
									<Button size="sm" icon="refresh" loading={refreshingIdentity === row.id} onclick={() => refreshIdentity(row.id)}>Refresh</Button>
								{/if}
								<Button size="sm" variant="danger-soft" icon="unlink" loading={unlinkingIdentity === row.id} onclick={() => unlink(row)}>Unlink</Button>
							{:else}
								<Button size="sm" variant="primary" icon="link" href={providerApi.startUrl(row.id, 'link')} external>Link</Button>
							{/if}
						</div>
					</li>
				{/each}
			</ul>
		{/if}
	</section>
</div>

<Dialog bind:open={tokenDialog} title="New API token" busy={mintingToken}>
	<form id="token-form" class="stack" onsubmit={mintToken} novalidate>
		{#if tokenError}
			<Alert tone="danger" message={tokenError} onclose={() => (tokenError = null)} />
		{/if}
		<Field label="Name" for="token-name" hint="What this token is for, so you can tell it apart later.">
			<input id="token-name" class="input" bind:value={tokenName} maxlength={TOKEN_NAME_MAX} required />
		</Field>
		<fieldset class="scopes">
			<legend class="label">Scopes</legend>
			<p class="hint">Only what your role allows can be given.</p>
			{#each PERMISSIONS as permission (permission)}
				{@const allowed = granted.includes(permission)}
				<label class={['checkbox', !allowed && 'disabled']}>
					<input
						type="checkbox"
						checked={tokenScopes.includes(permission)}
						disabled={!allowed}
						onchange={(e) => toggleScope(permission, (e.currentTarget as HTMLInputElement).checked)}
					/>
					<span>
						<span class="strong">{PERMISSION_LABELS[permission].label}</span>
						<span class="hint">{allowed ? PERMISSION_LABELS[permission].description : 'Your role does not allow this.'}</span>
					</span>
				</label>
			{/each}
		</fieldset>
		<Field label="Expires" for="token-expiry" error={customDaysProblem}>
			<div class="expiry">
				<select id="token-expiry" class="select" bind:value={tokenExpiry}>
					<option value="7">In 7 days</option>
					<option value="30">In 30 days</option>
					<option value="90">In 90 days</option>
					<option value="365">In a year</option>
					<option value="custom">In a number of days…</option>
					<option value="never">Never</option>
				</select>
				{#if tokenExpiry === 'custom'}
					<input class="input" type="number" min="1" step="1" bind:value={tokenCustomDays} placeholder="Days" aria-label="Days until expiry" />
				{/if}
			</div>
		</Field>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (tokenDialog = false)} disabled={mintingToken}>Cancel</Button>
		<Button variant="primary" type="submit" loading={mintingToken} disabled={!tokenReady} onclick={() => document.querySelector<HTMLFormElement>('#token-form')?.requestSubmit()}>Mint token</Button>
	{/snippet}
</Dialog>

{#if minted}
	<Dialog open title={`Token “${minted.token.name}”`} description="Copy the secret now. It is shown this once and cannot be recovered." onclose={() => (minted = null)}>
		<div class="stack">
			<Alert tone="warn" message="Anyone holding this secret acts as you, within the token's scopes." />
			<div class="secret">
				<code class="secret-text">{minted.secret}</code>
				<CopyButton text={minted.secret} variant="primary" size="md" />
			</div>
			<p class="hint">
				Send it as <code>Authorization: Bearer {minted.secret.slice(0, 8)}…</code>.
				{#if minted.token.expires_at}It expires <Time value={minted.token.expires_at} />.{:else}It never expires.{/if}
			</p>
		</div>
		{#snippet footer()}
			<Button variant="primary" onclick={() => (minted = null)}>I have copied it</Button>
		{/snippet}
	</Dialog>
{/if}

<style>
	.profile {
		display: flex;
		gap: 18px;
		align-items: flex-start;
	}

	.profile-text {
		min-width: 0;
		flex: 1;
	}

	.username {
		font-size: 18px;
		overflow-wrap: anywhere;
	}

	.perm {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 2px 9px;
		border-radius: 999px;
		font-size: 12px;
		font-weight: 500;
	}

	.perm.on {
		background: var(--ok-soft);
		color: var(--ok-text);
	}

	.perm.off {
		background: var(--surface-3);
		color: var(--text-3);
	}

	.narrow {
		max-width: 440px;
	}

	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.providers {
		list-style: none;
		margin: 0;
		padding: 0;
	}

	.provider {
		display: flex;
		gap: 14px;
		align-items: flex-start;
		padding: 14px 20px;
		border-bottom: 1px solid var(--border);
	}

	.provider:last-child {
		border-bottom: none;
	}

	.provider-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 40px;
		height: 40px;
		border-radius: 11px;
		background: var(--surface-3);
		color: var(--text-2);
		flex: none;
	}

	.provider-text {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}

	.provider-actions {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		justify-content: flex-end;
	}

	.scopes {
		border: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 10px;
	}

	.scopes legend {
		padding: 0;
		font-size: 13px;
		font-weight: 500;
		margin-bottom: 2px;
	}

	.expiry {
		display: grid;
		grid-template-columns: 1fr;
		gap: 6px;
	}

	.expiry:has(input) {
		grid-template-columns: 1fr 120px;
	}

	.secret {
		display: flex;
		gap: 8px;
		align-items: stretch;
	}

	.secret-text {
		flex: 1;
		min-width: 0;
		padding: 10px 12px;
		border-radius: var(--radius-sm);
		background: var(--surface-2);
		border: 1px solid var(--border);
		font-size: 13px;
		overflow-wrap: anywhere;
		user-select: all;
	}

	@media (max-width: 640px) {
		.provider {
			flex-wrap: wrap;
		}

		.provider-actions {
			width: 100%;
			justify-content: flex-start;
		}
	}
</style>
