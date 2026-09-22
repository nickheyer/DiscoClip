<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import { page } from '$app/state';
	import type { PageData } from './$types';
	import { front, isApiError, messageOf } from '$lib/api';
	import { providerIcon } from '$lib/auth-messages';
	import Alert from '$lib/components/Alert.svelte';
	import Button from '$lib/components/Button.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import { SECRET_LABELS, frontCallbackMessage } from '$lib/frontends';
	import { clock } from '$lib/state/clock.svelte';
	import { onMount } from 'svelte';

	let { data }: { data: PageData } = $props();

	const info = $derived(data.info);
	const access = $derived(info.access);
	const next = $derived.by(() => {
		const wanted = page.url.searchParams.get('next');
		const home = `/f/${data.slug}`;
		if (!wanted || !wanted.startsWith(`${home}`) || wanted.startsWith(`${home}/login`)) return home;
		return wanted;
	});
	const flowError = $derived(frontCallbackMessage(page.url.searchParams.get('error')));
	const anyWay = $derived(access.secret !== null || access.accounts || access.providers.length > 0);

	onMount(() => {
		clock.start();
		return () => clock.stop();
	});

	$effect(() => {
		if (info.viewer || access.open) void goto(next, { replaceState: true });
	});

	let secret = $state('');
	let username = $state('');
	let password = $state('');
	let submitting = $state(false);
	let error = $state<string | null>(null);
	let retryAt = $state<number | null>(null);
	const wait = $derived(retryAt ? Math.max(0, Math.ceil((retryAt - clock.now) / 1000)) : 0);

	async function attempt(body: { secret?: string; username?: string; password?: string }) {
		submitting = true;
		error = null;
		try {
			await front.login(data.slug, body);
			await invalidate(`front:${data.slug}`);
			await goto(next);
		} catch (cause) {
			if (isApiError(cause)) {
				if (cause.status === 429) {
					retryAt = Date.now() + (cause.retryAfter ?? 60) * 1000;
				} else if (cause.status === 401) {
					error = body.secret !== undefined ? 'That is not it.' : 'Wrong username or password.';
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

	function withSecret(event: SubmitEvent) {
		event.preventDefault();
		if (secret.trim() === '' || wait > 0) return;
		void attempt({ secret: secret.trim() });
	}

	function withAccount(event: SubmitEvent) {
		event.preventDefault();
		if (username.trim() === '' || password === '' || wait > 0) return;
		void attempt({ username: username.trim(), password });
	}
</script>

<svelte:head>
	<title>Log in · {info.name}</title>
</svelte:head>

<div class="login">
	<div class="card box">
		<div class="card-body stack">
			<div>
				<h1>{info.name}</h1>
				<p class="muted">{info.description || 'Log in to look around.'}</p>
			</div>
			{#if flowError}
				<Alert tone="danger" message={flowError} />
			{/if}
			{#if error}
				<Alert tone="danger" message={error} onclose={() => (error = null)} />
			{/if}
			{#if wait > 0}
				<Alert tone="warn" message={`Too many attempts. Try again in ${wait}s.`} />
			{/if}
			{#if !anyWay}
				<Alert tone="warn" message="Login is unavailable for this site." />
			{/if}

			{#if access.secret}
				<form class="stack" onsubmit={withSecret}>
					<Field label={SECRET_LABELS[access.secret].label} for="fl-secret">
						<PasswordInput id="fl-secret" bind:value={secret} placeholder={SECRET_LABELS[access.secret].prompt} autocomplete="off" disabled={submitting} mono={access.secret !== 'password'} />
					</Field>
					<Button type="submit" variant="primary" block loading={submitting} disabled={secret.trim() === '' || wait > 0}>Enter</Button>
				</form>
			{/if}

			{#if access.accounts}
				{#if access.secret}<div class="or faint small">or with an account</div>{/if}
				<form class="stack" onsubmit={withAccount}>
					<Field label="Username" for="fl-username">
						<input id="fl-username" class="input" bind:value={username} autocomplete="username" disabled={submitting} />
					</Field>
					<Field label="Password" for="fl-password">
						<PasswordInput id="fl-password" bind:value={password} autocomplete="current-password" disabled={submitting} />
					</Field>
					<Button type="submit" variant="primary" block loading={submitting} disabled={username.trim() === '' || password === '' || wait > 0}>Log in</Button>
				</form>
			{/if}

			{#if access.providers.length}
				{#if access.secret || access.accounts}<div class="or faint small">or sign in with</div>{/if}
				<div class="stack-sm">
					{#each access.providers as provider (provider.id)}
						<Button variant="secondary" block href={front.startUrl(data.slug, provider.id)} external>
							<Icon name={providerIcon(provider.id)} size={16} /> {provider.name}
						</Button>
					{/each}
				</div>
				{#if access.discord_members}
					<p class="muted small">Discord login requires membership in this site’s server.</p>
				{/if}
			{/if}
		</div>
	</div>
</div>

<style>
	.login {
		display: flex;
		justify-content: center;
		padding: 24px 0;
	}

	.box {
		width: 100%;
		max-width: 420px;
	}

	h1 {
		margin: 0 0 4px;
		font-size: 22px;
	}

	.muted {
		margin: 0;
	}

	.or {
		text-align: center;
	}
</style>
