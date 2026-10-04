<script lang="ts">
	import PlayIcon from '@lucide/svelte/icons/play';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { platforms as platformsApi } from '$lib/api/endpoints';
	import type { MediaKind, PlatformCoverage, PlatformHealth, SessionSupport } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PlatformSummary, { HEALTH } from '$lib/components/PlatformSummary.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { mediaLabel, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const POLL = 10_000;
	const HEALTHS: PlatformHealth[] = ['working', 'failing', 'login_required', 'unknown'];

	let platforms = $state<PlatformCoverage[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let checking = $state(false);
	let filter = $state('');
	let tag = $state('');
	let media = $state<MediaKind | ''>('');
	let sessionFilter = $state<SessionSupport | ''>('');

	/** The verdict filter lives in the URL, so the health page can point at the failing ones. */
	const health = $derived.by((): PlatformHealth | '' => {
		const wanted = page.url.searchParams.get('health');
		return wanted && HEALTHS.includes(wanted as PlatformHealth) ? (wanted as PlatformHealth) : '';
	});

	async function pickHealth(value: string) {
		// The path is this page's own route; only the query changes.
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		await goto(value ? `${resolve('/platforms')}?health=${value}` : resolve('/platforms'), {
			keepFocus: true,
			noScroll: true,
			replaceState: true
		});
	}

	async function load(quiet = false) {
		if (!quiet) {
			loading = true;
			error = null;
		}
		try {
			platforms = await platformsApi.list();
		} catch (err) {
			if (!quiet) error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void load();
		const timer = setInterval(() => {
			if (platforms.some((p) => p.running)) void load(true);
		}, POLL);
		return () => clearInterval(timer);
	});

	async function checkAll() {
		checking = true;
		try {
			const started = await platformsApi.checkAll();
			notify.success(`Checking ${number(started.platforms.length)} platforms`);
			await load(true);
		} catch (err) {
			reportError(err, 'Could not start the checks');
		} finally {
			checking = false;
		}
	}

	const tags = $derived([...new Set(platforms.flatMap((p) => p.tags))].sort());
	const shown = $derived(
		platforms.filter((p) => {
			const needle = filter.trim().toLowerCase();
			if (
				needle &&
				!p.name.toLowerCase().includes(needle) &&
				!p.id.includes(needle) &&
				!p.hosts.some((h) => h.includes(needle))
			)
				return false;
			if (tag && !p.tags.includes(tag)) return false;
			if (media && !p.media.includes(media)) return false;
			if (sessionFilter && p.session !== sessionFilter) return false;
			if (health && p.health !== health) return false;
			return true;
		})
	);
	const running = $derived(platforms.filter((p) => p.running).length);
	const failing = $derived(platforms.filter((p) => p.health === 'failing').length);
	const busy = $derived(checking || running > 0);
</script>

<PageHeader
	title="Platforms"
	description="Every site the server can fetch from, and whether each still works: by its check links, and by the jobs that finish on it."
>
	{#snippet actions()}
		{#if session.can('manage_jobs')}
			<button
				type="button"
				class="btn preset-filled-primary-500"
				onclick={checkAll}
				disabled={busy}
				aria-busy={busy}
			>
				{#if busy}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
				{busy ? 'Checking…' : 'Check all'}
			</button>
		{/if}
	{/snippet}
</PageHeader>

<Card label="Filters">
	<form class="space-y-3" onsubmit={(event) => event.preventDefault()}>
		<div class="flex flex-wrap gap-2">
			<SearchInput
				bind:value={filter}
				placeholder="Name or host"
				debounce={0}
				class="min-w-56 flex-1"
			/>
			<select class="select w-40" bind:value={tag} aria-label="Tag">
				<option value="">Every tag</option>
				{#each tags as t (t)}<option value={t}>{t}</option>{/each}
			</select>
			<select class="select w-36" bind:value={media} aria-label="Media">
				<option value="">Any media</option>
				{#each ['video', 'audio', 'image', 'file'] as const as kind (kind)}
					<option value={kind}>{mediaLabel(kind)}</option>
				{/each}
			</select>
			<select class="select w-40" bind:value={sessionFilter} aria-label="Login">
				<option value="">Any login</option>
				<option value="none">No login</option>
				<option value="optional">Login optional</option>
				<option value="required">Login required</option>
			</select>
			<select
				class="select w-40"
				value={health}
				onchange={(event) => pickHealth(event.currentTarget.value)}
				aria-label="Verdict"
			>
				<option value="">Any verdict</option>
				{#each HEALTHS as verdict (verdict)}
					<option value={verdict}>{HEALTH[verdict].label}</option>
				{/each}
			</select>
		</div>
		<p class="text-sm text-surface-600-400">
			{number(shown.length)} of {number(platforms.length)} platforms
			{#if running > 0}· {number(running)} checking{/if}
			{#if failing > 0}
				· <a href="{resolve('/platforms')}?health=failing" class="anchor text-error-600-400"
					>{number(failing)} failing</a
				>
			{/if}
		</p>
	</form>
</Card>

{#if error && !loading}
	<ErrorState {error} onretry={() => load()} />
{:else if loading && platforms.length === 0}
	<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3" aria-busy="true">
		{#each { length: 6 }, i (i)}
			<div class="h-36 placeholder animate-pulse"></div>
		{/each}
	</div>
{:else if shown.length === 0}
	<EmptyState title="No platform matches" description="Loosen the filters to see more." />
{:else}
	<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
		{#each shown as platform (platform.id)}
			<a
				href={resolve('/(app)/platforms/[id]', { id: platform.id })}
				class="min-w-0 card preset-filled-surface-100-900 p-4"
			>
				<PlatformSummary {platform} />
			</a>
		{/each}
	</div>
{/if}
