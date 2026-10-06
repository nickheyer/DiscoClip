<script lang="ts">
	import DownloadIcon from '@lucide/svelte/icons/download';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import { front } from '$lib/api/endpoints';
	import type {
		FrontInfo,
		FrontJob,
		FrontJobQuery,
		FrontPage,
		FrontPlatform,
		JobOrder,
		MediaKind
	} from '$lib/api/types';
	import { takeInline } from '$lib/front/inline';
	import Count from '$lib/components/Count.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import FrontHeader from '$lib/components/front/FrontHeader.svelte';
	import Bytes from '$lib/components/Bytes.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import { mediaLabel } from '$lib/format';
	import { reportError } from '$lib/toast.svelte';

	const LIMIT = 48;
	const KINDS: MediaKind[] = ['video', 'audio', 'image', 'file'];

	const slug = $derived(page.params.slug ?? '');

	let info = $state<FrontInfo | null>(null);
	let jobs = $state<FrontJob[]>([]);
	let next = $state<string | null>(null);
	let total = $state<number | null>(null);
	let platforms = $state<FrontPlatform[]>([]);
	let loading = $state(true);
	let loadingMore = $state(false);
	let error = $state<unknown>(null);
	let q = $state('');
	let media = $state<MediaKind | ''>('');
	let resolver = $state('');
	let order = $state<JobOrder>('newest');
	let sentinel = $state<HTMLElement | null>(null);

	/** The filters as a query, with the cursor in the place the order reads it from. */
	function queryFor(cursor: string | null): FrontJobQuery {
		return {
			q: q.trim() || undefined,
			media: media || undefined,
			resolver: resolver || undefined,
			order,
			before: order === 'newest' && cursor ? cursor : undefined,
			after: order === 'oldest' && cursor ? cursor : undefined,
			limit: LIMIT
		};
	}

	function toLogin() {
		return goto(resolve('/(front)/f/[slug]/login', { slug }));
	}

	/** Whether the filters are at rest, which is the page the server sends inline */
	function unfiltered(): boolean {
		return !q.trim() && !media && !resolver && order === 'newest';
	}

	/** The view and its first page, from the page itself when it carries them, else from the API */
	async function fetchBoth(): Promise<[FrontInfo, FrontPage]> {
		const inline = takeInline(slug);
		if (!inline) {
			return Promise.all([front.info(slug), front.jobs(slug, queryFor(null))]);
		}
		if (inline.page && unfiltered()) return [inline.info, inline.page];
		if (!inline.info.access.open && !inline.info.viewer) {
			throw new ApiError(401, 'This view asks for a login.');
		}
		return [inline.info, await front.jobs(slug, queryFor(null))];
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [loaded, first] = await fetchBoth();
			if (current !== requestId) return;
			info = loaded;
			document.title = loaded.name;
			jobs = first.jobs;
			next = first.next;
			total = first.total;
			platforms = first.platforms;
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
			const pageResult = await front.jobs(slug, queryFor(next));
			jobs = [...jobs, ...pageResult.jobs];
			next = pageResult.next;
			total = pageResult.total;
			platforms = pageResult.platforms;
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

{#snippet placeholders()}
	<div class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4" aria-busy="true">
		{#each { length: 8 }, i (i)}
			<div class="aspect-video placeholder animate-pulse"></div>
		{/each}
	</div>
{/snippet}

{#if info}
	<FrontHeader {info} onlogout={logout} />
{/if}

<main class="mx-auto max-w-7xl space-y-4 p-4">
	{#if error && !loading}
		<ErrorState {error} onretry={load} />
	{:else if !info}
		{@render placeholders()}
	{:else}
		<form
			class="flex flex-wrap gap-2 card preset-filled-surface-100-900 p-3"
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
				{#each platforms as platform (platform.id)}
					<option value={platform.id}>{platform.name}</option>
				{/each}
			</select>
			<select
				class="select w-36"
				bind:value={order}
				onchange={() => void load()}
				aria-label="Order"
			>
				<option value="newest">Newest first</option>
				<option value="oldest">Oldest first</option>
			</select>
		</form>

		{#if total !== null}
			<p class="text-sm text-surface-600-400"><Count value={total} noun="item" /></p>
		{/if}

		{#if loading && jobs.length === 0}
			{@render placeholders()}
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
					<li class="group flex flex-col overflow-hidden card preset-filled-surface-100-900">
						<a href={resolve('/(front)/f/[slug]/j/[id]', { slug, id: job.id })} class="block">
							<div class="relative flex aspect-video items-center justify-center preset-tonal">
								{#if job.thumbnail}
									<img
										src={job.thumbnail}
										alt=""
										class="h-full w-full object-cover transition group-hover:scale-[1.02]"
										loading="lazy"
									/>
								{:else}
									<MediaKindIcon kind={job.media} class="size-10 text-surface-600-400" />
								{/if}
								{#if job.duration_secs !== null}
									<span
										class="absolute right-2 bottom-2 badge preset-filled-surface-950-50 tabular-nums"
										style="--badge-size: var(--text-xs)"
										><Duration value={job.duration_secs} /></span
									>
								{/if}
								{#if job.recording}
									<span
										class="absolute top-2 left-2 badge animate-pulse preset-filled-error-500"
										style="--badge-size: var(--text-xs)">Recording</span
									>
								{:else if job.live}
									<span
										class="absolute top-2 left-2 badge preset-filled-error-500"
										style="--badge-size: var(--text-xs)">Live</span
									>
								{/if}
							</div>
							<div class="space-y-1 p-3">
								<p class="line-clamp-2 min-h-[2lh] font-medium" title={job.title ?? ''}>
									{job.title ?? mediaLabel(job.media)}
								</p>
								<p class="flex items-center gap-1.5 truncate text-sm text-surface-600-400">
									<MediaKindIcon kind={job.media} class="size-3.5" />
									{[job.uploader, job.platform].filter(Boolean).join(' · ')}
								</p>
							</div>
						</a>
						<div
							class="mt-auto flex items-center justify-between px-3 pb-3 text-sm text-surface-600-400"
						>
							<span><Timestamp at={job.published_at} /> · <Bytes value={job.size} /></span>
							{#if info.downloads && job.download_url}
								<a
									href={job.download_url}
									class="btn-icon btn-icon-sm hover:preset-tonal"
									title="Download"
									aria-label="Download {job.title ?? 'file'}"
									download
								>
									<DownloadIcon />
								</a>
							{/if}
						</div>
					</li>
				{/each}
			</ul>
			{#if next}
				<div bind:this={sentinel} class="flex justify-center py-6">
					{#if loadingMore}
						<Spinner class="[--size:1.5rem]" />
					{:else}
						<button type="button" class="btn preset-tonal" onclick={more}>Load more</button>
					{/if}
				</div>
			{/if}
		{/if}
	{/if}
</main>
