<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { ApiError } from '$lib/api/client';
	import Field from '$lib/components/Field.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { session } from '$lib/session.svelte';
	import { reportError } from '$lib/toast.svelte';

	let username = $state('');
	let key = $state('');
	let password = $state('');
	let confirm = $state('');
	let pending = $state(false);
	let error = $state<string | null>(null);

	const mismatch = $derived(confirm.length > 0 && confirm !== password);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (mismatch) return;
		pending = true;
		error = null;
		try {
			await session.recover(username.trim(), key.trim(), password);
			await goto(resolve('/'));
		} catch (err) {
			if (err instanceof ApiError && (err.status === 403 || err.status === 404)) {
				error = 'The key or the username is wrong.';
			} else if (err instanceof ApiError && err.rateLimited) {
				error = err.message;
			} else {
				reportError(err, 'Could not recover the account');
			}
		} finally {
			pending = false;
		}
	}
</script>

<svelte:head>
	<title>Recover · DiscoClip</title>
</svelte:head>

<div class="space-y-1">
	<h1 class="h4">Recover an account</h1>
	<p class="text-sm text-surface-600-400">
		The server prints a recovery key on its console each time it starts. Entering it sets a new
		password, ends the account's sessions and lifts its lockout. The key changes once used.
	</p>
</div>

<form class="space-y-4" onsubmit={submit}>
	<Field label="Username" for="recover-username" required>
		<input
			id="recover-username"
			class="input"
			type="text"
			autocomplete="username"
			bind:value={username}
			required
		/>
	</Field>
	<Field label="Recovery key" for="recover-key" required {error}>
		<input
			id="recover-key"
			class="input font-mono"
			type="text"
			autocomplete="off"
			spellcheck="false"
			bind:value={key}
			required
		/>
	</Field>
	<Field label="New password" for="recover-password" required>
		<input
			id="recover-password"
			class="input"
			type="password"
			autocomplete="new-password"
			bind:value={password}
			required
		/>
	</Field>
	<Field
		label="Confirm password"
		for="recover-confirm"
		required
		error={mismatch ? 'The passwords differ.' : null}
	>
		<input
			id="recover-confirm"
			class="input"
			type="password"
			autocomplete="new-password"
			bind:value={confirm}
			required
		/>
	</Field>
	<button type="submit" class="btn w-full preset-filled-primary-500" disabled={pending || mismatch}>
		{#if pending}<Spinner />{/if}
		Set password and log in
	</button>
</form>

<p class="text-center text-sm text-surface-600-400">
	<a class="anchor" href={resolve('/login')}>Back to log in</a>
</p>
