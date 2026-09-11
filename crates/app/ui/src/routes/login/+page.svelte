<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { PageData } from './$types';
	import { auth, isApiError, messageOf, providers as providerApi } from '$lib/api';
	import { callbackErrorMessage, providerIcon } from '$lib/auth-messages';
	import Alert from '$lib/components/Alert.svelte';
	import AuthCard from '$lib/components/AuthCard.svelte';
	import Button from '$lib/components/Button.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import { safeNext } from '$lib/format';
	import { clock } from '$lib/state/clock.svelte';
	import { session } from '$lib/state/session.svelte';

	let { data }: { data: PageData } = $props();

	let username = $state('');
	let password = $state('');
	let submitting = $state(false);
	let error = $state<string | null>(null);
	let retryAt = $state<number | null>(null);

	const next = $derived(safeNext(page.url.searchParams.get('next')));
	const flowError = $derived(callbackErrorMessage(page.url.searchParams.get('error')));
	const wait = $derived(retryAt ? Math.max(0, Math.ceil((retryAt - clock.now) / 1000)) : 0);
	const ready = $derived(username.trim() !== '' && password !== '' && wait === 0);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		submitting = true;
		error = null;
		try {
			const me = await auth.login({ username: username.trim(), password });
			session.set(me);
			await goto(next);
		} catch (cause) {
			if (isApiError(cause)) {
				if (cause.status === 429) {
					retryAt = Date.now() + (cause.retryAfter ?? 60) * 1000;
					error = null;
				} else if (cause.status === 401) {
					error = 'Wrong username or password.';
				} else {
					error = cause.message;
				}
			} else {
				error = messageOf(cause);
			}
			password = '';
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head>
	<title>Log in · DiscoClip</title>
</svelte:head>

<AuthCard title="Log in">
	<div class="stack">
		{#if flowError}
			<Alert tone="danger" message={flowError} />
		{/if}
		<form class="stack" onsubmit={submit} novalidate>
			{#if error}
				<Alert tone="danger" message={error} onclose={() => (error = null)} />
			{/if}
			{#if wait > 0}
				<Alert tone="warn" message={`Too many wrong passwords. Try again in ${wait}s.`} />
			{/if}
			<Field label="Username" for="login-username">
				<!-- svelte-ignore a11y_autofocus -->
				<input
					id="login-username"
					class="input"
					bind:value={username}
					autocomplete="username"
					spellcheck="false"
					required
					autofocus
				/>
			</Field>
			<Field label="Password" for="login-password">
				<PasswordInput id="login-password" bind:value={password} autocomplete="current-password" required />
			</Field>
			<Button type="submit" variant="primary" size="lg" block loading={submitting} disabled={!ready}>
				Log in
			</Button>
		</form>

		{#if data.providers.length}
			<div class="divider"><span>or continue with</span></div>
			<div class="providers">
				{#each data.providers as provider (provider.id)}
					<a class="provider" href={providerApi.startUrl(provider.id, 'login')} data-sveltekit-reload>
						<Icon name={providerIcon(provider.id)} size={17} />
						<span>{provider.name}</span>
					</a>
				{/each}
			</div>
		{/if}
	</div>
</AuthCard>

<style>
	.divider {
		display: flex;
		align-items: center;
		gap: 12px;
		color: var(--text-3);
		font-size: 12.5px;
	}

	.divider::before,
	.divider::after {
		content: '';
		flex: 1;
		height: 1px;
		background: var(--border);
	}

	.providers {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}

	.provider {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 10px;
		min-height: 40px;
		padding: 0 14px;
		border: 1px solid var(--border-strong);
		border-radius: var(--radius-sm);
		background: var(--surface);
		color: var(--text);
		font-weight: 500;
		text-decoration: none;
		transition: background-color 0.12s;
	}

	.provider:hover {
		background: var(--surface-2);
		text-decoration: none;
	}
</style>
