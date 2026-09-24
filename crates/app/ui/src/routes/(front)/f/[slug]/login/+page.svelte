<script lang="ts">
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import { front } from '$lib/api/endpoints';
	import type { FrontInfo } from '$lib/api/types';
	import Mark from '$lib/brand/Mark.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { reportError } from '$lib/toast.svelte';

	const slug = $derived(page.params.slug ?? '');

	let info = $state<FrontInfo | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let secret = $state('');
	let username = $state('');
	let password = $state('');
	let pending = $state<'secret' | 'account' | null>(null);
	let failure = $state<string | null>(null);

	const REASONS: Record<string, string> = {
		state: 'The login could not be verified. Try again.',
		denied: 'The provider denied the login.',
		provider: 'The provider returned an error.',
		exchange: 'The provider did not accept the login code.',
		identity: 'The provider did not say who you are.',
		frontend: 'This view no longer accepts that login.',
		not_listed: 'Your account is not on the list for this view.',
		not_member: 'Your Discord account is not in every server this view covers.',
		guilds: 'Your Discord servers could not be read.'
	};
	const callbackError = $derived.by(() => {
		const code = page.url.searchParams.get('error');
		return code ? (REASONS[code] ?? 'The login did not go through.') : null;
	});

	const gallery = $derived(resolve('/(front)/f/[slug]', { slug }));

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const loaded = await front.info(slug);
			if (current !== requestId) return;
			info = loaded;
			document.title = `Log in · ${loaded.name}`;
			if (loaded.access.open || loaded.viewer) await goto(gallery);
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void slug;
		void load();
	});

	async function submit(kind: 'secret' | 'account', event: SubmitEvent) {
		event.preventDefault();
		pending = kind;
		failure = null;
		try {
			await front.login(
				slug,
				kind === 'secret' ? { secret } : { username: username.trim(), password }
			);
			await goto(gallery);
		} catch (err) {
			if (err instanceof ApiError && (err.status === 401 || err.status === 400)) {
				failure = kind === 'secret' ? 'That is not right.' : 'Wrong username or password.';
			} else if (err instanceof ApiError && err.rateLimited) {
				failure = err.message;
			} else {
				reportError(err, 'Could not log in');
			}
		} finally {
			pending = null;
		}
	}

	const SECRET_LABEL = { pin: 'PIN', password: 'Password', token: 'Access token' } as const;
</script>

<main class="flex min-h-dvh flex-col items-center justify-center gap-6 p-6">
	{#if error && !loading}
		<ErrorState {error} title="This view could not be opened" onretry={load} />
	{:else if !info}
		<Spinner class="size-8" />
	{:else}
		<div class="flex items-center gap-3">
			<Mark size={36} title="" class="text-primary-500" />
			<div>
				<h1 class="h4">{info.name}</h1>
				{#if info.description}<p class="text-sm text-surface-600-400">{info.description}</p>{/if}
			</div>
		</div>

		<section
			class="w-full max-w-md space-y-6 card bg-surface-100-900 p-6 shadow-lg"
			aria-label="Log in"
		>
			{#if callbackError}
				<p class="card preset-tonal-error p-3 text-sm" role="alert">{callbackError}</p>
			{/if}

			{#if !info.access.secret && !info.access.accounts && info.access.providers.length === 0}
				<p class="text-sm text-surface-600-400">
					This view is closed. Nobody can log in right now.
				</p>
			{/if}

			{#if info.access.secret}
				{@const kind = info.access.secret}
				<form class="space-y-4" onsubmit={(event) => submit('secret', event)}>
					<Field
						label={SECRET_LABEL[kind]}
						for="front-secret"
						required
						error={pending === null ? failure : null}
					>
						<input
							id="front-secret"
							class="input font-mono"
							type={kind === 'pin' ? 'text' : 'password'}
							inputmode={kind === 'pin' ? 'numeric' : undefined}
							autocomplete={kind === 'password' ? 'current-password' : 'off'}
							bind:value={secret}
							required
						/>
					</Field>
					<button
						type="submit"
						class="btn w-full preset-filled-primary-500"
						disabled={pending !== null}
					>
						{#if pending === 'secret'}<Spinner />{/if}
						Enter
					</button>
				</form>
			{/if}

			{#if info.access.accounts}
				{#if info.access.secret}
					<div class="flex items-center gap-3 text-sm text-surface-600-400">
						<hr class="hr flex-1" />
						or with an account
						<hr class="hr flex-1" />
					</div>
				{/if}
				<form class="space-y-4" onsubmit={(event) => submit('account', event)}>
					<Field label="Username" for="front-username" required>
						<input
							id="front-username"
							class="input"
							type="text"
							autocomplete="username"
							bind:value={username}
							required
						/>
					</Field>
					<Field
						label="Password"
						for="front-password"
						required
						error={pending === null && info.access.secret === null ? failure : null}
					>
						<input
							id="front-password"
							class="input"
							type="password"
							autocomplete="current-password"
							bind:value={password}
							required
						/>
					</Field>
					<button
						type="submit"
						class="btn w-full preset-filled-primary-500"
						disabled={pending !== null}
					>
						{#if pending === 'account'}<Spinner />{/if}
						Log in
					</button>
				</form>
			{/if}

			{#if info.access.providers.length > 0}
				{#if info.access.secret || info.access.accounts}
					<div class="flex items-center gap-3 text-sm text-surface-600-400">
						<hr class="hr flex-1" />
						or
						<hr class="hr flex-1" />
					</div>
				{/if}
				<div class="space-y-2">
					{#each info.access.providers as provider (provider.id)}
						<a href={front.providerStartUrl(slug, provider.id)} class="btn w-full preset-tonal"
							>Continue with {provider.name}</a
						>
					{/each}
					{#if info.access.discord_members}
						<p class="text-center text-sm text-surface-600-400">
							Discord logins must belong to every server this view covers.
						</p>
					{/if}
				</div>
			{/if}
		</section>
	{/if}
</main>
