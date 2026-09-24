<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import Field from '$lib/components/Field.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { session } from '$lib/session.svelte';
	import { reportError } from '$lib/toast.svelte';

	let username = $state('');
	let password = $state('');
	let confirm = $state('');
	let pending = $state(false);

	const mismatch = $derived(confirm.length > 0 && confirm !== password);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (mismatch) return;
		pending = true;
		try {
			await session.setup(username.trim(), password);
			await goto(resolve('/'));
		} catch (error) {
			reportError(error, 'Could not create the account');
		} finally {
			pending = false;
		}
	}
</script>

<svelte:head>
	<title>Set up · DiscoClip</title>
</svelte:head>

<div class="space-y-1">
	<h1 class="h4">Create the admin account</h1>
	<p class="text-sm text-surface-600-400">
		This is the first account on the server. It manages everything else.
	</p>
</div>

<form class="space-y-4" onsubmit={submit}>
	<Field label="Username" for="setup-username" required>
		<input
			id="setup-username"
			class="input"
			type="text"
			autocomplete="username"
			bind:value={username}
			required
		/>
	</Field>
	<Field label="Password" for="setup-password" required>
		<input
			id="setup-password"
			class="input"
			type="password"
			autocomplete="new-password"
			bind:value={password}
			required
		/>
	</Field>
	<Field
		label="Confirm password"
		for="setup-confirm"
		required
		error={mismatch ? 'The passwords differ.' : null}
	>
		<input
			id="setup-confirm"
			class="input"
			type="password"
			autocomplete="new-password"
			bind:value={confirm}
			required
		/>
	</Field>
	<button type="submit" class="btn w-full preset-filled-primary-500" disabled={pending || mismatch}>
		{#if pending}<Spinner />{/if}
		Create account
	</button>
</form>
