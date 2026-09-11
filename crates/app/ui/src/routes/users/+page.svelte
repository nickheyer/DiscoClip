<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { ROLES, messageOf, users } from '$lib/api';
	import type { Role } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { ROLE_LABELS } from '$lib/permissions';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { USERNAME_MAX, matchProblem, passwordProblem, usernameProblem } from '$lib/validation';

	let { data }: { data: PageData } = $props();

	let query = $state('');
	const shown = $derived(
		data.users.filter((u) => u.username.toLowerCase().includes(query.trim().toLowerCase()))
	);

	let dialog = $state(false);
	let username = $state('');
	let role = $state<Role>('viewer');
	let withPassword = $state(true);
	let password = $state('');
	let confirmation = $state('');
	let creating = $state(false);
	let error = $state<string | null>(null);

	const usernameError = $derived(usernameProblem(username));
	const passwordError = $derived(withPassword ? passwordProblem(password) : null);
	const confirmError = $derived(withPassword ? matchProblem(password, confirmation) : null);
	const ready = $derived(
		username !== '' &&
			!usernameError &&
			(!withPassword || (password !== '' && confirmation === password && !passwordError))
	);

	function open() {
		username = '';
		role = 'viewer';
		withPassword = true;
		password = '';
		confirmation = '';
		error = null;
		dialog = true;
	}

	async function create(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		creating = true;
		error = null;
		try {
			const user = await users.create({
				username: username.trim(),
				role,
				password: withPassword ? password : undefined
			});
			dialog = false;
			toast.ok(`Created ${user.username}.`);
			await invalidate('app:users');
			await goto(`/users/${user.id}`);
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			creating = false;
		}
	}
</script>

<svelte:head>
	<title>Accounts · DiscoClip</title>
</svelte:head>

<PageHeader title="Accounts" description="Who can log in to DiscoClip and what their role allows.">
	{#snippet actions()}
		<Button variant="primary" icon="plus" onclick={open}>New account</Button>
	{/snippet}
</PageHeader>

<div class="stack">
	<div class="row-between">
		<input class="input search" type="search" placeholder="Filter by username" bind:value={query} aria-label="Filter accounts" />
		<span class="faint small">{shown.length} of {data.users.length}</span>
	</div>

	<div class="table-wrap">
		<table class="table">
			<thead>
				<tr>
					<th>Account</th>
					<th>Role</th>
					<th>Password</th>
					<th>Created</th>
					<th>Updated</th>
					<th></th>
				</tr>
			</thead>
			<tbody>
				{#each shown as user (user.id)}
					<tr>
						<td>
							<a href={`/users/${user.id}`} class="row row-link">
								<Avatar name={user.username} size={28} />
								<span>{user.username}</span>
								{#if user.id === session.user?.id}<Badge tone="info" size="sm">You</Badge>{/if}
							</a>
						</td>
						<td><Badge tone={user.role === 'admin' ? 'accent' : 'neutral'} size="sm">{ROLE_LABELS[user.role].label}</Badge></td>
						<td>{#if user.has_password}Set{:else}<span class="faint">none · provider login only</span>{/if}</td>
						<td><Time value={user.created_at} /></td>
						<td><Time value={user.updated_at} /></td>
						<td class="actions"><Button size="sm" variant="ghost" href={`/users/${user.id}`} iconRight="chevron-right">Manage</Button></td>
					</tr>
				{:else}
					<tr><td colspan="6" class="faint">No account matches.</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
</div>

<Dialog bind:open={dialog} title="New account" busy={creating}>
	<form id="user-form" class="stack" onsubmit={create} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<Field label="Username" for="new-username" error={usernameError} hint={`Up to ${USERNAME_MAX} letters, digits, '.', '_' or '-'.`}>
			<input id="new-username" class="input" bind:value={username} autocomplete="off" spellcheck="false" maxlength={USERNAME_MAX} required aria-invalid={usernameError ? 'true' : undefined} />
		</Field>
		<fieldset class="roles">
			<legend>Role</legend>
			{#each ROLES as option (option)}
				<label class="radio">
					<input type="radio" name="role" value={option} bind:group={role} />
					<span>
						<span class="strong">{ROLE_LABELS[option].label}</span>
						<span class="hint">{ROLE_LABELS[option].description}</span>
					</span>
				</label>
			{/each}
		</fieldset>
		<label class="checkbox">
			<input type="checkbox" bind:checked={withPassword} />
			<span>
				<span class="strong">Set a password</span>
				<span class="hint">Without one the account logs in through a linked provider only.</span>
			</span>
		</label>
		{#if withPassword}
			<Field label="Password" for="new-password" error={passwordError} hint="8 to 256 characters.">
				<PasswordInput id="new-password" bind:value={password} autocomplete="new-password" invalid={!!passwordError} required />
			</Field>
			<Field label="Confirm password" for="new-confirm" error={confirmError}>
				<PasswordInput id="new-confirm" bind:value={confirmation} autocomplete="new-password" invalid={!!confirmError} required />
			</Field>
		{/if}
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={creating}>Cancel</Button>
		<Button variant="primary" loading={creating} disabled={!ready} onclick={() => document.querySelector<HTMLFormElement>('#user-form')?.requestSubmit()}>Create account</Button>
	{/snippet}
</Dialog>

<style>
	.search {
		max-width: 320px;
	}

	.roles {
		border: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 10px;
	}

	.roles legend {
		padding: 0;
		font-size: 13px;
		font-weight: 500;
		margin-bottom: 4px;
	}
</style>
