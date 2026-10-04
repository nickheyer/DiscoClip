<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import { providers } from '$lib/api/endpoints';
	import type { CallbackError } from '$lib/api/types';
	import Field from '$lib/components/Field.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { session } from '$lib/session.svelte';
	import { reportError } from '$lib/toast.svelte';

	let { data } = $props();

	let username = $state('');
	let password = $state('');
	let pending = $state(false);
	let error = $state<string | null>(null);

	const CALLBACK_ERRORS: Record<CallbackError, string> = {
		state: 'The login could not be verified. Try again.',
		denied: 'The provider denied the login.',
		provider: 'The provider returned an error.',
		identity: 'The provider did not say who you are.',
		exchange: 'The provider did not accept the login code.',
		session: 'The session could not be started.',
		already_linked: 'That provider account is linked to a different account.',
		provider_linked: 'Your account already has that provider linked.',
		unknown_identity:
			'No account is linked to that provider login. Log in with a password, then link it under Account.'
	};

	const callbackError = $derived.by(() => {
		const code = page.url.searchParams.get('error');
		return code && code in CALLBACK_ERRORS ? CALLBACK_ERRORS[code as CallbackError] : null;
	});

	/** Where to go after the login: a path on this site, else the dashboard. */
	const next = $derived.by(() => {
		const raw = page.url.searchParams.get('next') ?? '';
		return raw.startsWith('/') && !raw.startsWith('//') ? raw : resolve('/');
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		pending = true;
		error = null;
		try {
			await session.login(username.trim(), password);
			// `next` is a path on this site, checked above.
			// eslint-disable-next-line svelte/no-navigation-without-resolve
			await goto(next);
		} catch (err) {
			if (err instanceof ApiError && err.status === 401) {
				error = 'Wrong username or password.';
			} else if (err instanceof ApiError && err.rateLimited) {
				error = err.message;
			} else {
				reportError(err, 'Could not log in');
			}
		} finally {
			pending = false;
		}
	}
</script>

<svelte:head>
	<title>Log in · DiscoClip</title>
</svelte:head>

<div class="space-y-1">
	<h1 class="h4">Log in</h1>
	<p class="text-sm text-surface-600-400">Use your DiscoClip account.</p>
</div>

{#if callbackError}
	<p class="card preset-tonal-error p-3 text-sm" role="alert">{callbackError}</p>
{/if}

<form class="space-y-4" onsubmit={submit}>
	<Field label="Username" for="login-username" required>
		<input
			id="login-username"
			class="input"
			type="text"
			autocomplete="username"
			bind:value={username}
			required
		/>
	</Field>
	<Field label="Password" for="login-password" required {error}>
		<input
			id="login-password"
			class="input"
			type="password"
			autocomplete="current-password"
			bind:value={password}
			required
		/>
	</Field>
	<button type="submit" class="btn w-full preset-filled-primary-500" disabled={pending}>
		{#if pending}<Spinner />{/if}
		Log in
	</button>
</form>

{#if data.providers.length > 0}
	<div class="flex items-center gap-3 text-sm text-surface-600-400">
		<hr class="hr flex-1" />
		or
		<hr class="hr flex-1" />
	</div>
	<div class="space-y-2">
		{#each data.providers as provider (provider.id)}
			<a href={providers.startUrl(provider.id, 'login')} class="btn w-full preset-tonal">
				Continue with {provider.name}
			</a>
		{/each}
	</div>
{/if}

<p class="text-center text-sm text-surface-600-400">
	Locked out? <a class="anchor" href={resolve('/recover')}>Recover an account</a>
</p>
