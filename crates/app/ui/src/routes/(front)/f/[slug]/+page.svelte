<script lang="ts">
	import DownloadIcon from '@lucide/svelte/icons/download';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import { front } from '$lib/api/endpoints';
	import type { FrontInfo, FrontJob, MediaKind } from '$lib/api/types';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import FrontHeader from '$lib/components/front/FrontHeader.svelte';
	import { bytes, clock, mediaLabel } from '$lib/format';
	import { reportError } from '$lib/toast.svelte';

	const LIMIT = 48;
	const KINDS: MediaKind[] = ['video', 'audio', 'image', 'file'];

	const slug = $derived(page.params.slug ?? '');

	let info = $state<FrontInfo | null>(null);
	let jobs = $state<FrontJob[]>([]);
	let next = $state<string | null>(null);
	let loading = $state(true);
	let loadingMore = $state(false);
	let error = $state<unknown>(null);
	let q = $state('');
	let media = $state<MediaKind | ''>('');
	let resolver = $state('');
	let sentinel = $state<HTMLElement | null>(null);

	const resolvers = $derived([...new Set(jobs.map((job) => job.resolver))].sort());

	function toLogin() {
		return goto(resolve('/(front)/f/[slug]/login', { slug }));
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const loaded = await front.info(slug);
			if (current !== requestId) return;
			info = loaded;
			document.title = loaded.name;
			if (!loaded.access.open && !loaded.viewer) {
				await toLogin();
				return;
			}
			const first = await front.jobs(slug, {
				q: q.trim() || undefined,
				media: media || undefined,
				resolver: resolver || undefined,
				limit: LIMIT
			});
			if (current !== requestId) return;
			jobs = first.jobs;
			next = first.next;
		} catch (err) {
			if (current !== requestId) return;
			if (err instanceof ApiError && err.unauthorized) {
				await toLogin();
				return;
			}
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	async function more() {
		if (!next || loadingMore) return;
		loadingMore = true;
		try {
			const pageResult = await front.jobs(slug, {
				q: q.trim() || undefined,
				media: media || undefined,
				resolver: resolver || undefined,
				before: next,
				limit: LIMIT
			});
			jobs = [...jobs, ...pageResult.jobs];
			next = pageResult.next;
		} catch (err) {
			if (err instanceof ApiError && err.unauthorized) {
				await toLogin();
				return;
			}
			reportError(err, 'Could not load more');
		} finally {
			loadingMore = false;
		}
	}

	$effect(() => {
		void slug;
		void load();
	});

	onMount(() => {
		const observer = new IntersectionObserver((entries) => {
			if (entries.some((entry) => entry.isIntersecting)) void more();
		});
		$effect(() => {
			if (sentinel) observer.observe(sentinel);
			return () => {
				if (sentinel) observer.unobserve(sentinel);
			};
		});
		return () => observer.disconnect();
	});

	async function logout() {
		try {
			await front.logout(slug);
		} finally {
			await toLogin();
		}
	}
</script>

{#if info}
	<FrontHeader {info} onlogout={logout} />
{/if}

<main class="mx-auto max-w-7xl space-y-4 p-4">
	{#if error && !loading}
		<ErrorState {error} onretry={load} />
	{:else if !info}
		<div
			class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4"
			aria-busy="true"
		>
			{#each { length: 8 }, i (i)}
				<div class="aspect-video placeholder animate-pulse"></div>
			{/each}
		</div>
	{:else}
		<form
			class="flex flex-wrap gap-2"
			onsubmit={(event) => {
				event.preventDefault();
				void load();
			}}
		>
			<SearchInput
				bind:value={q}
				placeholder="Search titles and uploaders"
				onsearch={() => void load()}
				class="min-w-64 flex-1"
			/>
			<select
				class="select w-40"
				bind:value={media}
				onchange={() => void load()}
				aria-label="Media kind"
			>
				<option value="">Any media</option>
				{#each KINDS as kind (kind)}
					<option value={kind}>{mediaLabel(kind)}</option>
				{/each}
			</select>
			<select
				class="select w-44"
				bind:value={resolver}
				onchange={() => void load()}
				aria-label="Platform"
			>
				<option value="">Any platform</option>
				{#each resolvers as id (id)}
					<option value={id}>{id}</option>
				{/each}
			</select>
		</form>

		{#if loading && jobs.length === 0}
			<div
				class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4"
				aria-busy="true"
			>
				{#each { length: 8 }, i (i)}
					<div class="aspect-video placeholder animate-pulse"></div>
				{/each}
			</div>
		{:else if jobs.length === 0}
			<EmptyState
				title="Nothing to show"
				description={q || media || resolver
					? 'Nothing matches these filters.'
					: 'Media appears here once jobs finish.'}
			/>
		{:else}
			<ul class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
				{#each jobs as job (job.id)}
					<li class="group overflow-hidden card bg-surface-100-900">
						<a href={resolve('/(front)/f/[slug]/j/[id]', { slug, id: job.id })} class="block">
							<div
								class="relative flex aspect-video items-center justify-center bg-surface-200-800 text-surface-600-400"
							>
								{#if job.thumbnail}
									<img
										src={job.thumbnail}
										alt=""
										class="h-full w-full object-cover transition group-hover:scale-[1.02]"
										loading="lazy"
									/>
								{:else}
									<MediaKindIcon kind={job.media} class="size-10" />
								{/if}
								{#if job.duration_secs !== null}
									<span
										class="absolute right-2 bottom-2 rounded-base bg-black/70 px-1.5 py-0.5 text-sm text-white tabular-nums"
										>{clock(job.duration_secs)}</span
									>
								{/if}
								{#if job.live}
									<span
										class="absolute top-2 left-2 text-sm font-semibold text-white drop-shadow-md"
										>Live</span
									>
								{/if}
							</div>
							<div class="space-y-1 p-3">
								<p class="line-clamp-2 font-medium" title={job.title ?? ''}>
									{job.title ?? mediaLabel(job.media)}
								</p>
								<p class="flex items-center gap-1.5 truncate text-sm text-surface-600-400">
									<MediaKindIcon kind={job.media} class="size-3.5" />
									{[job.uploader, job.resolver].filter(Boolean).join(' · ')}
								</p>
							</div>
						</a>
						<div class="flex items-center justify-between px-3 pb-3 text-sm text-surface-600-400">
							<span><RelativeTime at={job.published_at} /> · {bytes(job.size)}</span>
							{#if info.downloads && job.download_url}
								<a
									href={job.download_url}
									class="btn-icon btn-icon-sm hover:preset-tonal"
									title="Download"
									aria-label="Download {job.title ?? 'file'}"
									download
								>
									<DownloadIcon class="size-4" />
								</a>
							{/if}
						</div>
					</li>
				{/each}
			</ul>
			{#if next}
				<div bind:this={sentinel} class="flex justify-center py-6">
					{#if loadingMore}
						<Spinner class="size-6" />
					{:else}
						<button type="button" class="btn preset-tonal" onclick={more}>Load more</button>
					{/if}
				</div>
			{/if}
		{/if}
	{/if}
</main>
