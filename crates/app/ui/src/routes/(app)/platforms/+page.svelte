<script lang="ts">
	import PlayIcon from '@lucide/svelte/icons/play';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { platforms as platformsApi } from '$lib/api/endpoints';
	import type { MediaKind, PlatformCoverage, SessionSupport } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PlatformSummary from '$lib/components/PlatformSummary.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { mediaLabel, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const POLL = 10_000;

	let platforms = $state<PlatformCoverage[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let checking = $state(false);
	let filter = $state('');
	let tag = $state('');
	let media = $state<MediaKind | ''>('');
	let sessionFilter = $state<SessionSupport | ''>('');
	let health = $state<'' | 'failing' | 'passing' | 'never'>('');

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
			notify.success(`Running checks on ${number(started.platforms.length)} platforms`);
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
			if (health === 'failing' && p.failed === 0) return false;
			if (health === 'passing' && (p.failed > 0 || p.passed === 0)) return false;
			if (health === 'never' && p.last_run_at !== null) return false;
			return true;
		})
	);
	const running = $derived(platforms.filter((p) => p.running).length);
	const failing = $derived(platforms.filter((p) => p.failed > 0).length);
	const busy = $derived(checking || running > 0);
</script>

<PageHeader
	title="Platforms"
	description="Every site the server can fetch from, with its latest link checks."
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
				{busy ? 'Running checks…' : 'Run all checks'}
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
			<select class="select w-36" bind:value={health} aria-label="Link check result">
				<option value="">Any result</option>
				<option value="failing">Failing</option>
				<option value="passing">All pass</option>
				<option value="never">Never run</option>
			</select>
		</div>
		<p class="text-sm text-surface-600-400">
			{number(shown.length)} of {number(platforms.length)} platforms
			{#if running > 0}· {number(running)} running checks{/if}
			{#if failing > 0}· <span class="text-error-600-400">{number(failing)} failing</span>{/if}
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
