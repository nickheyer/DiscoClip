<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import { page } from '$app/state';
	import type { LayoutData } from './$types';
	import { front, messageOf } from '$lib/api';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import { theme } from '$lib/state/theme.svelte';
	import { onMount } from 'svelte';

	let { data, children }: { data: LayoutData; children: import('svelte').Snippet } = $props();

	const info = $derived(data.info);
	const onLogin = $derived(page.url.pathname.endsWith('/login'));
	let leaving = $state(false);
	let logoutError = $state<string | null>(null);

	onMount(() => {
		theme.init();
	});

	async function logout() {
		leaving = true;
		logoutError = null;
		try {
			await front.logout(data.slug);
			await invalidate(`front:${data.slug}`);
			await goto(`/f/${data.slug}`);
		} catch (cause) {
			logoutError = messageOf(cause);
		} finally {
			leaving = false;
		}
	}

	const themeIcon = $derived(
		theme.mode === 'dark' ? 'moon' : theme.mode === 'light' ? 'sun' : 'monitor'
	);
</script>

<svelte:head>
	<title>{info.name}</title>
</svelte:head>

<div class="front">
	<header class="bar">
		<a class="brand" href={`/f/${data.slug}`}>
			<span class="logo"><Icon name="video" size={15} /></span>
			<span class="name">{info.name}</span>
		</a>
		<div class="right">
			<button type="button" class="icon-btn" onclick={() => theme.cycle()} title="Theme" aria-label="Theme">
				<Icon name={themeIcon} size={16} />
			</button>
			{#if info.viewer}
				<span class="who small"><Icon name="user" size={14} /> {info.viewer.display}</span>
				<Button size="sm" variant="ghost" icon="logout" loading={leaving} onclick={logout}>Log out</Button>
			{:else if !info.access.open && !onLogin}
				<Button size="sm" variant="primary" icon="log-in" href={`/f/${data.slug}/login`}>Log in</Button>
			{/if}
		</div>
	</header>
	{#if logoutError}
		<p class="error-text small centred">{logoutError}</p>
	{/if}
	<main class="body">
		{@render children()}
	</main>
	<footer class="foot faint small">Served by DiscoClip</footer>
</div>

<style>
	.front {
		min-height: 100vh;
		display: flex;
		flex-direction: column;
		background: var(--bg);
		color: var(--text);
	}

	.bar {
		position: sticky;
		top: 0;
		z-index: 20;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 10px 20px;
		background: var(--surface);
		border-bottom: 1px solid var(--border);
	}

	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		font-weight: 600;
		font-size: 15px;
		color: var(--text);
		text-decoration: none;
		min-width: 0;
	}

	.name {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.logo {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 28px;
		height: 28px;
		border-radius: 8px;
		background: linear-gradient(135deg, var(--accent), color-mix(in srgb, var(--accent) 60%, #22d3ee));
		color: #fff;
		flex: none;
	}

	.right {
		display: flex;
		align-items: center;
		gap: 8px;
	}

	.who {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--text-2);
	}

	.icon-btn {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--text-2);
		cursor: pointer;
	}

	.icon-btn:hover {
		background: var(--surface-3);
		color: var(--text);
	}

	.body {
		flex: 1;
		width: 100%;
		max-width: 1240px;
		margin: 0 auto;
		padding: 24px 20px 48px;
		box-sizing: border-box;
	}

	.centred {
		text-align: center;
		margin: 8px 0 0;
	}

	.foot {
		text-align: center;
		padding: 16px;
	}
</style>
