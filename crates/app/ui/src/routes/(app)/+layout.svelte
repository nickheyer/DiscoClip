<script lang="ts">
	import ActivityIcon from '@lucide/svelte/icons/activity';
	import BotIcon from '@lucide/svelte/icons/bot';
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
	import MenuIcon from '@lucide/svelte/icons/menu';
	import MoonIcon from '@lucide/svelte/icons/moon';
	import GalleryHorizontalEndIcon from '@lucide/svelte/icons/gallery-horizontal-end';
	import GlobeIcon from '@lucide/svelte/icons/globe';
	import HeartPulseIcon from '@lucide/svelte/icons/heart-pulse';
	import LayoutDashboardIcon from '@lucide/svelte/icons/layout-dashboard';
	import ListVideoIcon from '@lucide/svelte/icons/list-video';
	import LogOutIcon from '@lucide/svelte/icons/log-out';
	import ScrollTextIcon from '@lucide/svelte/icons/scroll-text';
	import ServerIcon from '@lucide/svelte/icons/server';
	import SettingsIcon from '@lucide/svelte/icons/settings';
	import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal';
	import SunIcon from '@lucide/svelte/icons/sun';
	import TerminalIcon from '@lucide/svelte/icons/terminal';
	import UserIcon from '@lucide/svelte/icons/user';
	import UsersIcon from '@lucide/svelte/icons/users';
	import XIcon from '@lucide/svelte/icons/x';
	import { AppBar, Dialog, Menu, Navigation, Portal, Toast } from '@skeletonlabs/skeleton-svelte';
	import type { Component } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import type { Permission } from '$lib/api/types';
	import Wordmark from '$lib/brand/Wordmark.svelte';
	import SubmitDialog from '$lib/components/SubmitDialog.svelte';
	import { feed } from '$lib/events.svelte';
	import { mode } from '$lib/mode.svelte';
	import { session } from '$lib/session.svelte';
	import { submitDialog } from '$lib/submit.svelte';
	import { pageTitle } from '$lib/title.svelte';
	import { toaster } from '$lib/toast.svelte';

	let { data, children } = $props();

	interface Entry {
		label: string;
		href: string;
		icon: Component<{ class?: string }>;
		permission?: Permission;
		/** Shown only to accounts with a Discord identity. */
		discord?: boolean;
	}

	const ENTRIES: Entry[] = [
		{ label: 'Dashboard', href: resolve('/'), icon: LayoutDashboardIcon },
		{ label: 'Jobs', href: resolve('/jobs'), icon: ListVideoIcon },
		{
			label: 'Applications',
			href: resolve('/applications'),
			icon: BotIcon,
			permission: 'manage_applications'
		},
		{ label: 'Servers', href: resolve('/servers'), icon: ServerIcon, discord: true },
		{ label: 'Profiles', href: resolve('/profiles'), icon: SlidersHorizontalIcon },
		{
			label: 'Views',
			href: resolve('/views'),
			icon: GalleryHorizontalEndIcon,
			permission: 'manage_settings'
		},
		{ label: 'Platforms', href: resolve('/platforms'), icon: GlobeIcon },
		{ label: 'Users', href: resolve('/users'), icon: UsersIcon, permission: 'manage_users' },
		{
			label: 'Settings',
			href: resolve('/settings'),
			icon: SettingsIcon,
			permission: 'manage_settings'
		},
		{ label: 'Audit', href: resolve('/audit'), icon: ScrollTextIcon, permission: 'view_audit_log' },
		{ label: 'Health', href: resolve('/health'), icon: HeartPulseIcon },
		{ label: 'Metrics', href: resolve('/metrics'), icon: ActivityIcon },
		{ label: 'Logs', href: resolve('/logs'), icon: TerminalIcon, permission: 'view_logs' }
	];

	const entries = $derived(
		ENTRIES.filter(
			(entry) =>
				(!entry.permission || session.can(entry.permission)) && (!entry.discord || data.hasDiscord)
		)
	);

	const GROUPS = [
		{
			label: 'Workspace',
			paths: [resolve('/'), resolve('/jobs'), resolve('/applications'), resolve('/servers')]
		},
		{
			label: 'Configuration',
			paths: [resolve('/profiles'), resolve('/views'), resolve('/platforms')]
		},
		{ label: 'Administration', paths: [resolve('/users'), resolve('/settings')] },
		{
			label: 'Monitoring',
			paths: [resolve('/health'), resolve('/metrics'), resolve('/logs'), resolve('/audit')]
		}
	];
	const groups = $derived(
		GROUPS.map((group) => ({
			label: group.label,
			entries: group.paths.flatMap((href) => entries.filter((entry) => entry.href === href))
		})).filter((group) => group.entries.length > 0)
	);
	const barEntries = $derived(
		entries.filter((entry) =>
			[resolve('/'), resolve('/jobs'), resolve('/profiles')].some((href) => href === entry.href)
		)
	);
	const moreActive = $derived(
		entries.some((entry) => active(entry.href) && !barEntries.includes(entry)) ||
			active(resolve('/account'))
	);

	function active(href: string): boolean {
		const path = page.url.pathname;
		return href === resolve('/') ? path === href : path === href || path.startsWith(`${href}/`);
	}

	interface Crumb {
		label: string;
		/** Set on the section entry, which stays a link. */
		href?: string;
		/** Set on the last crumb, which names the page on show. */
		current?: boolean;
	}

	/** The navigation entry whose href is the longest match for this path, with its group. */
	const section = $derived.by(() => {
		let best: { group: string; entry: Entry } | null = null;
		for (const group of groups) {
			for (const entry of group.entries) {
				if (!active(entry.href)) continue;
				if (!best || entry.href.length > best.entry.href.length) {
					best = { group: group.label, entry };
				}
			}
		}
		return best;
	});

	const crumbs = $derived.by((): Crumb[] => {
		const leaf = pageTitle.value;
		if (!section) return leaf ? [{ label: leaf, current: true }] : [];
		if (!leaf || leaf === section.entry.label) {
			return [{ label: section.group }, { label: section.entry.label, current: true }];
		}
		return [
			{ label: section.group },
			{ label: section.entry.label, href: section.entry.href },
			{ label: leaf, current: true }
		];
	});

	let moreOpen = $state(false);

	$effect(() => {
		feed.start();
		return () => feed.stop();
	});

	const FEED_LABEL = {
		connecting: 'Connecting to live updates',
		live: 'Live updates on',
		offline: 'Live updates lost. Reconnecting'
	} as const;
	const FEED_DOT = {
		connecting: 'bg-warning-500',
		live: 'bg-success-500',
		offline: 'bg-error-500'
	} as const;

	async function onAccountSelect(value: string) {
		if (value === 'account') await goto(resolve('/account'));
		if (value === 'mode') mode.toggle();
		if (value === 'logout') await session.logout();
	}
</script>

<svelte:head>
	<title>{pageTitle.value ? `${pageTitle.value} · DiscoClip` : 'DiscoClip'}</title>
</svelte:head>

{#snippet navigationLinks()}
	{#each groups as group (group.label)}
		<Navigation.Group class="gap-2">
			<Navigation.Label class="px-3 text-sm font-semibold tracking-wide uppercase">
				{group.label}
			</Navigation.Label>
			<Navigation.Menu class="gap-1">
				{#each group.entries as entry (entry.href)}
					{@const Icon = entry.icon}
					<Navigation.TriggerAnchor
						href={entry.href}
						aria-current={active(entry.href) ? 'page' : undefined}
						onclick={() => (moreOpen = false)}
						class="min-h-11 gap-3 rounded-base border-l-[3px] px-3 py-2.5 {active(entry.href)
							? 'border-primary-500 bg-surface-200-800 font-semibold text-surface-950-50'
							: 'border-transparent text-surface-700-300 hover:preset-tonal'}"
					>
						<Icon class="size-5 shrink-0 {active(entry.href) ? 'text-primary-500' : ''}" />
						<Navigation.TriggerText class="text-base leading-6"
							>{entry.label}</Navigation.TriggerText
						>
					</Navigation.TriggerAnchor>
				{/each}
			</Navigation.Menu>
		</Navigation.Group>
	{/each}
{/snippet}

{#snippet connection()}
	<div class="flex items-center gap-2.5 text-sm text-surface-600-400" role="status">
		<span class="size-2 shrink-0 rounded-full {FEED_DOT[feed.state]}" aria-hidden="true"></span>
		<span>{FEED_LABEL[feed.state]}</span>
	</div>
{/snippet}

<a
	href="#main-content"
	class="sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[80] focus:rounded-base focus:preset-filled-primary-500 focus:px-4 focus:py-3"
	>Skip to content</a
>

<div class="min-h-dvh lg:grid lg:grid-cols-[16rem_minmax(0,1fr)]">
	<aside class="sticky top-0 hidden h-dvh border-r border-surface-200-800 lg:block">
		<Navigation
			layout="sidebar"
			role="navigation"
			aria-label="Main navigation"
			class="grid h-full w-full grid-rows-[auto_minmax(0,1fr)_auto] gap-0 overflow-hidden bg-surface-100-900 p-0"
		>
			<Navigation.Header class="flex h-20 items-center border-b border-surface-200-800 px-6">
				<a href={resolve('/')} aria-label="DiscoClip dashboard"><Wordmark size={32} /></a>
			</Navigation.Header>
			<Navigation.Content class="gap-6 overflow-y-auto px-3 py-6">
				{@render navigationLinks()}
			</Navigation.Content>
			<Navigation.Footer class="border-t border-surface-200-800 px-6 py-5">
				{@render connection()}
			</Navigation.Footer>
		</Navigation>
	</aside>

	<div class="min-w-0 pb-[calc(5rem+env(safe-area-inset-bottom))] lg:pb-0">
		<AppBar
			class="sticky top-0 z-30 border-b border-surface-200-800 bg-surface-50-950/95 p-0 backdrop-blur"
		>
			<AppBar.Toolbar
				class="h-16 grid-cols-[auto_minmax(0,1fr)_auto] gap-3 px-4 sm:px-6 lg:h-20 lg:grid-cols-[minmax(0,1fr)_auto] lg:px-8"
			>
				<AppBar.Lead class="lg:hidden" aria-label="Open navigation">
					<button
						type="button"
						class="btn-icon hover:preset-tonal"
						onclick={() => (moreOpen = true)}
						aria-label="Open navigation"
						aria-haspopup="dialog"
					>
						<MenuIcon />
					</button>
				</AppBar.Lead>
				<AppBar.Headline class="min-w-0">
					{#if crumbs.length > 0}
						<nav aria-label="Breadcrumb" class="min-w-0">
							<ol
								class="flex min-w-0 items-center gap-2 text-2xl font-semibold tracking-tight sm:gap-3 sm:text-3xl"
							>
								{#each crumbs as crumb, i (i)}
									<li
										class="min-w-0 items-center gap-2 sm:gap-3 {crumb.current
											? 'flex'
											: crumb.href
												? 'flex shrink-0'
												: 'hidden shrink-0 sm:flex'}"
									>
										{#if i > 0}
											<ChevronRightIcon
												class="size-6 shrink-0 text-surface-500 sm:size-7 {i === 1
													? 'hidden sm:block'
													: ''}"
												aria-hidden="true"
											/>
										{/if}
										{#if crumb.href}
											<a href={crumb.href} class="link-body font-normal text-surface-700-300"
												>{crumb.label}</a
											>
										{:else if crumb.current}
											<span class="truncate text-surface-950-50" aria-current="page"
												>{crumb.label}</span
											>
										{:else}
											<span class="font-normal text-surface-600-400">{crumb.label}</span>
										{/if}
									</li>
								{/each}
							</ol>
						</nav>
					{/if}
				</AppBar.Headline>
				<AppBar.Trail class="items-center" aria-label="Account">
					<Menu
						positioning={{ placement: 'bottom-end' }}
						onSelect={(details) => void onAccountSelect(details.value)}
					>
						<Menu.Trigger class="btn gap-2 hover:preset-tonal" aria-label="Account menu">
							<UserIcon />
							<span class="hidden max-w-48 truncate sm:inline">{session.user?.username}</span>
							<ChevronDownIcon class="size-4" aria-hidden="true" />
						</Menu.Trigger>
						<Portal>
							<Menu.Positioner class="z-40">
								<Menu.Content
									class="min-w-72 card border border-surface-200-800 bg-surface-100-900 p-2 shadow-xl"
								>
									<div class="flex items-center justify-between gap-6 px-3 py-2">
										<div class="min-w-0">
											<p class="truncate text-base font-semibold">{session.user?.username}</p>
											<p class="text-sm text-surface-600-400 capitalize">{session.user?.role}</p>
										</div>
										<Menu.Item value="account" class="btn shrink-0 gap-2 preset-tonal btn-sm">
											<UserIcon class="size-4" />
											<Menu.ItemText>Account</Menu.ItemText>
										</Menu.Item>
									</div>
									<Menu.Separator class="my-1 hr" />
									<Menu.Item
										value="mode"
										class="flex min-h-11 items-center justify-start gap-3 px-3 text-base"
									>
										{#if mode.current === 'dark'}
											<SunIcon class="size-5 shrink-0" />
										{:else}
											<MoonIcon class="size-5 shrink-0" />
										{/if}
										<Menu.ItemText class="grow text-left">{mode.label}</Menu.ItemText>
									</Menu.Item>
									<Menu.Item
										value="logout"
										class="flex min-h-11 items-center justify-start gap-3 px-3 text-base"
									>
										<LogOutIcon class="size-5 shrink-0" />
										<Menu.ItemText class="grow text-left">Log out</Menu.ItemText>
									</Menu.Item>
								</Menu.Content>
							</Menu.Positioner>
						</Portal>
					</Menu>
				</AppBar.Trail>
			</AppBar.Toolbar>
		</AppBar>

		<main
			id="main-content"
			tabindex="-1"
			class="mx-auto flex w-full max-w-[100rem] min-w-0 flex-col gap-6 p-4 sm:p-6 lg:gap-8 lg:p-8"
		>
			{@render children()}
		</main>
	</div>
</div>

<Navigation
	layout="bar"
	role="navigation"
	aria-label="Main navigation"
	class="fixed inset-x-0 bottom-0 z-30 border-t border-surface-200-800 bg-surface-100-900 px-3 pt-2 pb-[max(0.5rem,env(safe-area-inset-bottom))] lg:hidden"
>
	<Navigation.Menu class="grid grid-cols-4 gap-2">
		{#each barEntries as entry (entry.href)}
			{@const Icon = entry.icon}
			<Navigation.TriggerAnchor
				href={entry.href}
				aria-current={active(entry.href) ? 'page' : undefined}
				class="min-h-14 gap-1 rounded-base py-2 {active(entry.href)
					? 'bg-surface-200-800 font-semibold text-surface-950-50'
					: 'text-surface-700-300'}"
			>
				<Icon class="size-5 {active(entry.href) ? 'text-primary-500' : ''}" />
				<Navigation.TriggerText class="text-sm leading-5">{entry.label}</Navigation.TriggerText>
			</Navigation.TriggerAnchor>
		{/each}
		<Navigation.Trigger
			class="min-h-14 gap-1 rounded-base py-2 {moreActive
				? 'bg-surface-200-800 font-semibold text-surface-950-50'
				: 'text-surface-700-300'}"
			onclick={() => (moreOpen = true)}
			aria-haspopup="dialog"
			aria-label="More navigation"
		>
			<EllipsisIcon class="size-5 {moreActive ? 'text-primary-500' : ''}" />
			<Navigation.TriggerText class="text-sm leading-5">More</Navigation.TriggerText>
		</Navigation.Trigger>
	</Navigation.Menu>
</Navigation>

<Dialog open={moreOpen} onOpenChange={(details) => (moreOpen = details.open)}>
	<Portal>
		<Dialog.Backdrop class="fixed inset-0 z-40 bg-surface-950/60 backdrop-blur-sm" />
		<Dialog.Positioner class="fixed inset-0 z-40 flex justify-start">
			<Dialog.Content class="flex h-dvh w-80 max-w-[90vw] flex-col bg-surface-100-900 shadow-xl">
				<header
					class="flex min-h-20 shrink-0 items-center justify-between border-b border-surface-200-800 px-5"
				>
					<Dialog.Title><Wordmark size={30} /><span class="sr-only"> navigation</span></Dialog.Title
					>
					<Dialog.CloseTrigger class="btn-icon hover:preset-tonal" aria-label="Close navigation"
						><XIcon /></Dialog.CloseTrigger
					>
				</header>
				<Navigation
					layout="sidebar"
					role="navigation"
					aria-label="All navigation"
					class="h-auto min-h-0 w-full flex-1 overflow-y-auto px-3 py-6"
				>
					<Navigation.Content class="gap-6">
						{@render navigationLinks()}
						<Navigation.Group>
							<Navigation.Menu>
								<Navigation.TriggerAnchor
									href={resolve('/account')}
									onclick={() => (moreOpen = false)}
									aria-current={active(resolve('/account')) ? 'page' : undefined}
									class="min-h-11 gap-3 rounded-base border-l-[3px] px-3 py-2.5 {active(
										resolve('/account')
									)
										? 'border-primary-500 bg-surface-200-800 font-semibold text-surface-950-50'
										: 'border-transparent text-surface-700-300 hover:preset-tonal'}"
								>
									<UserIcon
										class="size-5 shrink-0 {active(resolve('/account')) ? 'text-primary-500' : ''}"
									/><Navigation.TriggerText class="text-base leading-6"
										>Account</Navigation.TriggerText
									>
								</Navigation.TriggerAnchor>
							</Navigation.Menu>
						</Navigation.Group>
					</Navigation.Content>
				</Navigation>
				<footer
					class="border-t border-surface-200-800 px-6 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))]"
				>
					{@render connection()}
				</footer>
			</Dialog.Content>
		</Dialog.Positioner>
	</Portal>
</Dialog>

{#if session.can('manage_jobs')}
	<SubmitDialog bind:open={submitDialog.open} />
{/if}

<Toast.Group {toaster}>
	{#snippet children(toast)}
		<Toast {toast} class="gap-3 p-4">
			<Toast.Message>
				<Toast.Title>{toast.title}</Toast.Title>
				<Toast.Description>{toast.description}</Toast.Description>
			</Toast.Message>
			<Toast.CloseTrigger aria-label="Dismiss notification" />
		</Toast>
	{/snippet}
</Toast.Group>
