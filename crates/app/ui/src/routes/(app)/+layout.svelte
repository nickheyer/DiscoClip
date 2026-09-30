<script lang="ts">
	import ActivityIcon from '@lucide/svelte/icons/activity';
	import BotIcon from '@lucide/svelte/icons/bot';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import DatabaseBackupIcon from '@lucide/svelte/icons/database-backup';
	import EllipsisIcon from '@lucide/svelte/icons/ellipsis';
	import GalleryHorizontalEndIcon from '@lucide/svelte/icons/gallery-horizontal-end';
	import GlobeIcon from '@lucide/svelte/icons/globe';
	import HeartPulseIcon from '@lucide/svelte/icons/heart-pulse';
	import LayoutDashboardIcon from '@lucide/svelte/icons/layout-dashboard';
	import ListVideoIcon from '@lucide/svelte/icons/list-video';
	import LogOutIcon from '@lucide/svelte/icons/log-out';
	import MenuIcon from '@lucide/svelte/icons/menu';
	import ScrollTextIcon from '@lucide/svelte/icons/scroll-text';
	import ServerIcon from '@lucide/svelte/icons/server';
	import SettingsIcon from '@lucide/svelte/icons/settings';
	import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal';
	import TerminalIcon from '@lucide/svelte/icons/terminal';
	import UserIcon from '@lucide/svelte/icons/user';
	import UsersIcon from '@lucide/svelte/icons/users';
	import XIcon from '@lucide/svelte/icons/x';
	import { AppBar, Dialog, Menu, Navigation, Portal } from '@skeletonlabs/skeleton-svelte';
	import type { Component } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import type { Permission } from '$lib/api/types';
	import Wordmark from '$lib/brand/Wordmark.svelte';
	import ModeToggle from '$lib/components/ModeToggle.svelte';
	import Status, { type Tone } from '$lib/components/Status.svelte';
	import SubmitDialog from '$lib/components/SubmitDialog.svelte';
	import { feed } from '$lib/events.svelte';
	import { session } from '$lib/session.svelte';
	import { submitDialog } from '$lib/submit.svelte';
	import { pageTitle } from '$lib/title.svelte';

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
		{
			label: 'Backups',
			href: resolve('/backups'),
			icon: DatabaseBackupIcon,
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
		{
			label: 'Administration',
			paths: [resolve('/users'), resolve('/settings'), resolve('/backups')]
		},
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

	let drawerOpen = $state(false);

	$effect(() => {
		feed.start();
		return () => feed.stop();
	});

	const FEED: Record<typeof feed.state, { label: string; tone: Tone }> = {
		connecting: { label: 'Connecting', tone: 'warning' },
		live: { label: 'Live updates', tone: 'success' },
		offline: { label: 'Reconnecting', tone: 'error' }
	};

	async function onAccountSelect(value: string) {
		if (value === 'account') await goto(resolve('/account'));
		if (value === 'logout') await session.logout();
	}

	const animBackdrop =
		'transition transition-discrete opacity-0 starting:data-[state=open]:opacity-0 data-[state=open]:opacity-100';
	const animDrawer =
		'transition transition-discrete opacity-0 -translate-x-full starting:data-[state=open]:opacity-0 starting:data-[state=open]:-translate-x-full data-[state=open]:opacity-100 data-[state=open]:translate-x-0';
</script>

<svelte:head>
	<title>{pageTitle.value ? `${pageTitle.value} · DiscoClip` : 'DiscoClip'}</title>
</svelte:head>

{#snippet links()}
	{#each groups as group (group.label)}
		<Navigation.Group>
			<Navigation.Label class="pl-2 font-semibold tracking-wide uppercase"
				>{group.label}</Navigation.Label
			>
			<Navigation.Menu>
				{#each group.entries as entry (entry.href)}
					{@const Icon = entry.icon}
					<Navigation.TriggerAnchor
						href={entry.href}
						aria-current={active(entry.href) ? 'page' : undefined}
						class={active(entry.href) ? 'preset-tonal-primary' : ''}
						onclick={() => (drawerOpen = false)}
					>
						<Icon class="size-4 shrink-0 {active(entry.href) ? 'text-primary-500' : ''}" />
						<Navigation.TriggerText>{entry.label}</Navigation.TriggerText>
					</Navigation.TriggerAnchor>
				{/each}
			</Navigation.Menu>
		</Navigation.Group>
	{/each}
{/snippet}

{#snippet connection()}
	<div class="px-2" role="status">
		<Status
			label={FEED[feed.state].label}
			tone={FEED[feed.state].tone}
			pulse={feed.state !== 'live'}
		/>
	</div>
{/snippet}

<a
	href="#main-content"
	class="sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[80] focus:btn focus:preset-filled-primary-500"
	>Skip to content</a
>

<div class="min-h-dvh lg:grid lg:grid-cols-[auto_minmax(0,1fr)]">
	<aside class="sticky top-0 hidden h-dvh lg:block">
		<Navigation
			layout="sidebar"
			aria-label="Main navigation"
			class="grid h-full grid-rows-[auto_minmax(0,1fr)_auto] gap-4 border-r border-surface-200-800"
		>
			<Navigation.Header class="px-2">
				<a href={resolve('/')} aria-label="DiscoClip dashboard"><Wordmark size={28} /></a>
			</Navigation.Header>
			<Navigation.Content class="min-h-0 overflow-y-auto">
				{@render links()}
			</Navigation.Content>
			<Navigation.Footer>
				{@render connection()}
			</Navigation.Footer>
		</Navigation>
	</aside>

	<div class="flex min-w-0 flex-col pb-[calc(5rem+env(safe-area-inset-bottom))] lg:pb-0">
		<AppBar
			class="sticky top-0 z-30 border-b border-surface-200-800 bg-surface-100-900/90 backdrop-blur"
		>
			<AppBar.Toolbar class="grid-cols-[auto_minmax(0,1fr)_auto] lg:grid-cols-[minmax(0,1fr)_auto]">
				<AppBar.Lead class="lg:hidden" aria-label="Open navigation">
					<button
						type="button"
						class="btn-icon hover:preset-tonal"
						onclick={() => (drawerOpen = true)}
						aria-label="Open navigation"
						aria-haspopup="dialog"
					>
						<MenuIcon />
					</button>
				</AppBar.Lead>
				<AppBar.Headline class="min-w-0">
					{#if crumbs.length > 0}
						<nav aria-label="Breadcrumb" class="min-w-0">
							<ol class="flex min-w-0 items-center gap-2">
								{#each crumbs as crumb, i (i)}
									<li
										class="min-w-0 items-center gap-2 {crumb.current
											? 'flex'
											: crumb.href
												? 'flex shrink-0'
												: 'hidden shrink-0 sm:flex'}"
									>
										{#if i > 0}
											<ChevronRightIcon
												class="size-4 shrink-0 text-surface-500 {i === 1 ? 'hidden sm:block' : ''}"
												aria-hidden="true"
											/>
										{/if}
										{#if crumb.href}
											<a href={crumb.href} class="anchor text-surface-600-400">{crumb.label}</a>
										{:else if crumb.current}
											<span class="truncate font-semibold" aria-current="page">{crumb.label}</span>
										{:else}
											<span class="text-surface-600-400">{crumb.label}</span>
										{/if}
									</li>
								{/each}
							</ol>
						</nav>
					{/if}
				</AppBar.Headline>
				<AppBar.Trail class="items-center gap-3" aria-label="Account">
					<ModeToggle />
					<Menu
						positioning={{ placement: 'bottom-end' }}
						onSelect={(details) => void onAccountSelect(details.value)}
					>
						<Menu.Trigger class="btn hover:preset-tonal" aria-label="Account menu">
							<UserIcon class="size-4" />
							<span class="hidden max-w-40 truncate sm:inline">{session.user?.username}</span>
							<ChevronDownIcon class="size-4" aria-hidden="true" />
						</Menu.Trigger>
						<Portal>
							<Menu.Positioner class="z-40">
								<Menu.Content>
									<Menu.ItemGroup>
										<Menu.ItemGroupLabel class="capitalize">
											{session.user?.username} · {session.user?.role}
										</Menu.ItemGroupLabel>
										<Menu.Item value="account" class="justify-start gap-2">
											<UserIcon class="size-4" />
											<Menu.ItemText>Account</Menu.ItemText>
										</Menu.Item>
									</Menu.ItemGroup>
									<Menu.Separator />
									<Menu.Item value="logout" class="justify-start gap-2">
										<LogOutIcon class="size-4" />
										<Menu.ItemText>Log out</Menu.ItemText>
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
			class="mx-auto flex w-full max-w-[100rem] min-w-0 flex-1 flex-col gap-6 p-4 sm:p-6 lg:p-8"
		>
			{@render children()}
		</main>
	</div>
</div>

<Navigation
	layout="bar"
	aria-label="Main navigation"
	class="fixed inset-x-0 bottom-0 z-30 border-t border-surface-200-800 pb-[max(0.5rem,env(safe-area-inset-bottom))] lg:hidden"
>
	<Navigation.Menu class="grid grid-cols-4 gap-2">
		{#each barEntries as entry (entry.href)}
			{@const Icon = entry.icon}
			<Navigation.TriggerAnchor
				href={entry.href}
				aria-current={active(entry.href) ? 'page' : undefined}
				class={active(entry.href) ? 'preset-tonal-primary' : ''}
			>
				<Icon class="size-5 {active(entry.href) ? 'text-primary-500' : ''}" />
				<Navigation.TriggerText class="text-xs">{entry.label}</Navigation.TriggerText>
			</Navigation.TriggerAnchor>
		{/each}
		<Navigation.Trigger
			class={moreActive ? 'preset-tonal-primary' : ''}
			onclick={() => (drawerOpen = true)}
			aria-haspopup="dialog"
			aria-label="More navigation"
		>
			<EllipsisIcon class="size-5 {moreActive ? 'text-primary-500' : ''}" />
			<Navigation.TriggerText class="text-xs">More</Navigation.TriggerText>
		</Navigation.Trigger>
	</Navigation.Menu>
</Navigation>

<Dialog open={drawerOpen} onOpenChange={(details) => (drawerOpen = details.open)}>
	<Portal>
		<Dialog.Backdrop
			class="fixed inset-0 z-40 bg-surface-50-950/50 backdrop-blur-sm {animBackdrop}"
		/>
		<Dialog.Positioner class="fixed inset-0 z-40 flex justify-start">
			<Dialog.Content class="h-dvh w-80 max-w-[85vw] shadow-xl {animDrawer}">
				<Navigation
					layout="sidebar"
					aria-label="All navigation"
					class="grid h-full w-full grid-rows-[auto_minmax(0,1fr)_auto] gap-4"
				>
					<Navigation.Header class="flex items-center justify-between px-2">
						<Dialog.Title
							><Wordmark size={28} /><span class="sr-only"> navigation</span></Dialog.Title
						>
						<Dialog.CloseTrigger class="btn-icon hover:preset-tonal" aria-label="Close navigation">
							<XIcon />
						</Dialog.CloseTrigger>
					</Navigation.Header>
					<Navigation.Content class="min-h-0 overflow-y-auto">
						{@render links()}
						<Navigation.Group>
							<Navigation.Menu>
								<Navigation.TriggerAnchor
									href={resolve('/account')}
									onclick={() => (drawerOpen = false)}
									aria-current={active(resolve('/account')) ? 'page' : undefined}
									class={active(resolve('/account')) ? 'preset-tonal-primary' : ''}
								>
									<UserIcon class="size-4 shrink-0" />
									<Navigation.TriggerText>Account</Navigation.TriggerText>
								</Navigation.TriggerAnchor>
							</Navigation.Menu>
						</Navigation.Group>
					</Navigation.Content>
					<Navigation.Footer class="pb-[env(safe-area-inset-bottom)]">
						{@render connection()}
					</Navigation.Footer>
				</Navigation>
			</Dialog.Content>
		</Dialog.Positioner>
	</Portal>
</Dialog>

{#if session.can('manage_jobs')}
	<SubmitDialog bind:open={submitDialog.open} />
{/if}
