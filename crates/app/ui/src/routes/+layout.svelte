<script lang="ts">
	import '../app.css';
	import favicon from '$lib/assets/favicon.svg';
	import { onMount } from 'svelte';
	import { navigating, page } from '$app/state';
	import { auth, isApiError, messageOf, onUnauthorized } from '$lib/api';
	import type { Permission } from '$lib/api';
	import Avatar from '$lib/components/Avatar.svelte';
	import ConfirmHost from '$lib/components/ConfirmHost.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Toaster from '$lib/components/Toaster.svelte';
	import { ROLE_LABELS } from '$lib/permissions';
	import { bots } from '$lib/state/bots.svelte';
	import { clock } from '$lib/state/clock.svelte';
	import { jobs } from '$lib/state/jobs.svelte';
	import { session } from '$lib/state/session.svelte';
	import { theme } from '$lib/state/theme.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { children } = $props();

	interface NavItem {
		href: string;
		label: string;
		icon: string;
		permission?: Permission;
	}

	const NAV: NavItem[] = [
		{ href: '/', label: 'Overview', icon: 'dashboard' },
		{ href: '/jobs', label: 'Jobs', icon: 'activity' },
		{ href: '/applications', label: 'Applications', icon: 'bot', permission: 'manage_applications' },
		{ href: '/rules', label: 'Watch rules', icon: 'rules', permission: 'manage_watch_rules' },
		{ href: '/guilds', label: 'My guilds', icon: 'server' },
		{ href: '/users', label: 'Accounts', icon: 'users', permission: 'manage_users' },
		{ href: '/audit', label: 'Audit log', icon: 'audit', permission: 'view_audit_log' },
		{ href: '/account', label: 'Account', icon: 'user' }
	];

	const me = $derived(session.me);
	const pathname = $derived(page.url.pathname);
	const bare = $derived(!me || pathname === '/login' || pathname === '/setup');
	const items = $derived(NAV.filter((item) => !item.permission || session.can(item.permission)));
	let navOpen = $state(false);
	let loggingOut = $state(false);

	function active(href: string): boolean {
		return pathname === href || (href !== '/' && pathname.startsWith(`${href}/`));
	}

	onMount(() => {
		theme.init();
		clock.start();
		onUnauthorized(() => {
			if (!session.me || session.ending) return;
			toast.info('Your session ended. Log in again to continue.');
			void session.leave(`${page.url.pathname}${page.url.search}`);
		});
		// A document the browser keeps in its back/forward cache must not hold the status
		// stream open, or it takes up one of the few connections the browser allows a host.
		const hide = () => {
			bots.stop();
			jobs.stop();
		};
		const show = (event: PageTransitionEvent) => {
			if (event.persisted && session.me) {
				bots.start();
				jobs.start();
			}
		};
		window.addEventListener('pagehide', hide);
		window.addEventListener('pageshow', show);
		return () => {
			window.removeEventListener('pagehide', hide);
			window.removeEventListener('pageshow', show);
			clock.stop();
			bots.stop();
			jobs.stop();
			onUnauthorized(null);
		};
	});

	$effect(() => {
		if (me) {
			bots.start();
			jobs.start();
		} else {
			bots.stop();
			jobs.stop();
		}
	});

	$effect(() => {
		void pathname;
		navOpen = false;
	});

	async function logout() {
		loggingOut = true;
		try {
			await auth.logout();
		} catch (error) {
			if (!(isApiError(error) && error.status === 401)) {
				toast.error(`Could not log out: ${messageOf(error)}`);
				loggingOut = false;
				return;
			}
		}
		await session.leave();
		loggingOut = false;
	}

	const themeIcon = $derived(
		theme.mode === 'dark' ? 'moon' : theme.mode === 'light' ? 'sun' : 'monitor'
	);
	const themeLabel = $derived(
		theme.mode === 'dark'
			? 'Theme: dark'
			: theme.mode === 'light'
				? 'Theme: light'
				: 'Theme: follows the system'
	);
	const feedLabel = $derived(
		bots.state === 'live'
			? 'Live'
			: bots.state === 'connecting'
				? 'Connecting'
				: bots.state === 'reconnecting'
					? 'Reconnecting'
					: 'Offline'
	);
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
</svelte:head>

{#if navigating.to}
	<div class="progress" aria-hidden="true"></div>
{/if}

{#if bare || !me}
	<div class="bare">{@render children()}</div>
{:else}
	<div class="shell">
		<a class="skip" href="#main">Skip to content</a>
		<header class="topbar">
			<button
				type="button"
				class="icon-btn"
				onclick={() => (navOpen = !navOpen)}
				aria-label="Menu"
				aria-expanded={navOpen}
				aria-controls="sidebar"
			>
				<Icon name={navOpen ? 'x' : 'menu'} size={18} />
			</button>
			<a href="/" class="brand">
				<span class="logo"><Icon name="video" size={15} /></span>
				<span>DiscoClip</span>
			</a>
		</header>

		<aside id="sidebar" class={['sidebar', navOpen && 'open']}>
			<a href="/" class="brand brand-side">
				<span class="logo"><Icon name="video" size={15} /></span>
				<span>DiscoClip</span>
			</a>
			<nav class="nav" aria-label="Main">
				{#each items as item (item.href)}
					<a
						href={item.href}
						class={['nav-item', active(item.href) && 'active']}
						aria-current={active(item.href) ? 'page' : undefined}
					>
						<Icon name={item.icon} size={17} />
						<span>{item.label}</span>
					</a>
				{/each}
			</nav>
			<div class="side-foot">
				<div class="feed" title="The bot status stream">
					<span class={['feed-dot', bots.state]} aria-hidden="true"></span>
					<span>{feedLabel}</span>
				</div>
				<div class="who">
					<a href="/account" class="who-link" title="Your account">
						<Avatar name={me.user.username} size={30} />
						<span class="who-text">
							<span class="strong truncate">{me.user.username}</span>
							<span class="faint small">{ROLE_LABELS[me.user.role].label}</span>
						</span>
					</a>
					<button
						type="button"
						class="icon-btn"
						onclick={() => theme.cycle()}
						title={themeLabel}
						aria-label={themeLabel}
					>
						<Icon name={themeIcon} size={16} />
					</button>
					<button
						type="button"
						class="icon-btn"
						onclick={logout}
						title="Log out"
						aria-label="Log out"
						disabled={loggingOut}
					>
						<Icon name="logout" size={16} />
					</button>
				</div>
			</div>
		</aside>
		{#if navOpen}
			<button type="button" class="scrim" onclick={() => (navOpen = false)} aria-label="Close menu"
			></button>
		{/if}

		<main id="main" class="main">
			<div class="content">{@render children()}</div>
		</main>
	</div>
{/if}

<Toaster />
<ConfirmHost />

<style>
	.progress {
		position: fixed;
		top: 0;
		left: 0;
		right: 0;
		height: 2px;
		z-index: 200;
		background: linear-gradient(90deg, transparent, var(--accent), transparent);
		background-size: 50% 100%;
		animation: slide 1s linear infinite;
	}

	@keyframes slide {
		from {
			background-position: -50% 0;
		}
		to {
			background-position: 150% 0;
		}
	}

	.bare {
		min-height: 100vh;
	}

	.shell {
		display: grid;
		grid-template-columns: var(--sidebar-w) minmax(0, 1fr);
		min-height: 100vh;
	}

	.skip {
		position: absolute;
		left: -999px;
		top: 8px;
		z-index: 300;
		padding: 8px 12px;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
	}

	.skip:focus {
		left: 8px;
	}

	.topbar {
		display: none;
	}

	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		font-weight: 600;
		font-size: 15px;
		color: var(--text);
		text-decoration: none;
	}

	.brand:hover {
		text-decoration: none;
	}

	.brand-side {
		padding: 18px 18px 14px;
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
		box-shadow: var(--shadow-sm);
	}

	.sidebar {
		position: sticky;
		top: 0;
		height: 100vh;
		display: flex;
		flex-direction: column;
		background: var(--surface);
		border-right: 1px solid var(--border);
	}

	.nav {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 4px 10px;
	}

	.nav-item {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 8px 10px;
		border-radius: var(--radius-sm);
		color: var(--text-2);
		font-weight: 500;
		text-decoration: none;
		transition:
			background-color 0.12s,
			color 0.12s;
	}

	.nav-item:hover {
		background: var(--surface-3);
		color: var(--text);
		text-decoration: none;
	}

	.nav-item.active {
		background: var(--accent-soft);
		color: var(--accent-text);
	}

	.side-foot {
		margin-top: auto;
		padding: 10px 12px 14px;
		border-top: 1px solid var(--border);
		display: flex;
		flex-direction: column;
		gap: 10px;
	}

	.feed {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 0 6px;
		font-size: 12px;
		color: var(--text-3);
	}

	.feed-dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--text-3);
	}

	.feed-dot.live {
		background: var(--ok);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--ok) 25%, transparent);
	}

	.feed-dot.connecting,
	.feed-dot.reconnecting {
		background: var(--warn);
		animation: blink 1.2s ease-in-out infinite;
	}

	@keyframes blink {
		50% {
			opacity: 0.35;
		}
	}

	.who {
		display: flex;
		align-items: center;
		gap: 4px;
	}

	.who-link {
		display: flex;
		align-items: center;
		gap: 10px;
		flex: 1;
		min-width: 0;
		padding: 4px 6px;
		border-radius: var(--radius-sm);
		color: inherit;
		text-decoration: none;
	}

	.who-link:hover {
		background: var(--surface-3);
		text-decoration: none;
	}

	.who-text {
		display: flex;
		flex-direction: column;
		min-width: 0;
		line-height: 1.25;
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
		flex: none;
	}

	.icon-btn:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--text);
	}

	.icon-btn:disabled {
		opacity: 0.5;
		cursor: default;
	}

	.main {
		min-width: 0;
	}

	.content {
		max-width: var(--content-w);
		margin: 0 auto;
		padding: 32px 36px 64px;
	}

	.scrim {
		display: none;
	}

	@media (max-width: 900px) {
		.shell {
			grid-template-columns: minmax(0, 1fr);
		}

		.topbar {
			position: sticky;
			top: 0;
			z-index: 40;
			display: flex;
			align-items: center;
			gap: 8px;
			padding: 8px 12px;
			background: var(--surface);
			border-bottom: 1px solid var(--border);
		}

		.sidebar {
			position: fixed;
			top: 0;
			left: 0;
			bottom: 0;
			z-index: 60;
			width: var(--sidebar-w);
			transform: translateX(-100%);
			transition: transform 0.18s ease-out;
			box-shadow: var(--shadow-lg);
		}

		.sidebar.open {
			transform: none;
		}

		.scrim {
			display: block;
			position: fixed;
			inset: 0;
			z-index: 50;
			border: none;
			background: rgb(10 12 18 / 0.45);
		}

		.content {
			padding: 20px 16px 48px;
		}
	}
</style>
