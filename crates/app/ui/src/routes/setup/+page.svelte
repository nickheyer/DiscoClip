<script lang="ts">
	import { goto } from '$app/navigation';
	import { auth, isApiError, messageOf } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import AuthCard from '$lib/components/AuthCard.svelte';
	import Button from '$lib/components/Button.svelte';
	import Field from '$lib/components/Field.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import { clock } from '$lib/state/clock.svelte';
	import { session } from '$lib/state/session.svelte';
	import { setupDone } from '$lib/state/setup';
	import { toast } from '$lib/state/toast.svelte';
	import { matchProblem, passwordProblem, usernameProblem } from '$lib/validation';

	let token = $state('');
	let username = $state('');
	let password = $state('');
	let confirmation = $state('');
	let submitting = $state(false);
	let error = $state<string | null>(null);
	let retryAt = $state<number | null>(null);

	const wait = $derived(retryAt ? Math.max(0, Math.ceil((retryAt - clock.now) / 1000)) : 0);
	const usernameError = $derived(usernameProblem(username));
	const passwordError = $derived(passwordProblem(password));
	const matchError = $derived(matchProblem(password, confirmation));
	const ready = $derived(
		token.trim() !== '' &&
			username !== '' &&
			password !== '' &&
			confirmation === password &&
			!usernameError &&
			!passwordError &&
			wait === 0
	);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		submitting = true;
		error = null;
		try {
			const me = await auth.setup({ username: username.trim(), password, token: token.trim() });
			session.set(me);
			setupDone();
			toast.ok(`Welcome, ${me.user.username}. Your admin account is ready.`);
			await goto('/');
		} catch (cause) {
			if (isApiError(cause)) {
				if (cause.status === 429) retryAt = Date.now() + (cause.retryAfter ?? 60) * 1000;
				if (cause.status === 409) {
					setupDone();
					error = 'An account exists already, so setup is over. Log in instead.';
				} else if (cause.status === 403) {
					error = 'That setup token is wrong. Copy the one the server printed when it started.';
				} else {
					error = cause.message;
				}
			} else {
				error = messageOf(cause);
			}
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head>
	<title>Set up · DiscoClip</title>
</svelte:head>

<AuthCard
	title="Create the admin account"
	subtitle="The server printed a setup token when it started without any account. Enter it here with the first admin's username and password."
>
	<form class="stack" onsubmit={submit} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		{#if wait > 0}
			<Alert tone="warn" message={`Too many wrong tokens. Try again in ${wait}s.`} />
		{/if}
		<Field label="Setup token" for="setup-token" hint="From the server log: “enter setup token …”.">
			<!-- svelte-ignore a11y_autofocus -->
			<input
				id="setup-token"
				class="input mono"
				bind:value={token}
				autocomplete="off"
				spellcheck="false"
				required
				autofocus
			/>
		</Field>
		<Field label="Username" for="setup-username" error={usernameError}>
			<input
				id="setup-username"
				class="input"
				bind:value={username}
				autocomplete="username"
				spellcheck="false"
				maxlength="32"
				required
				aria-invalid={usernameError ? 'true' : undefined}
			/>
		</Field>
		<Field label="Password" for="setup-password" error={passwordError} hint="8 to 256 characters.">
			<PasswordInput
				id="setup-password"
				bind:value={password}
				autocomplete="new-password"
				invalid={!!passwordError}
				required
			/>
		</Field>
		<Field label="Confirm password" for="setup-confirm" error={matchError}>
			<PasswordInput
				id="setup-confirm"
				bind:value={confirmation}
				autocomplete="new-password"
				invalid={!!matchError}
				required
			/>
		</Field>
		<Button type="submit" variant="primary" size="lg" block loading={submitting} disabled={!ready}>
			Create account and log in
		</Button>
		{#if error?.includes('exists already')}
			<p class="hint" style="text-align:center">
				<a href="/login">Go to the login page</a>
			</p>
		{/if}
	</form>
</AuthCard>
