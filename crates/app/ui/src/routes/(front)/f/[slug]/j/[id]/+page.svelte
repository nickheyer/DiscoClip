<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import DownloadIcon from '@lucide/svelte/icons/download';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { ApiError } from '$lib/api/client';
	import { front } from '$lib/api/endpoints';
	import type { FrontInfo, FrontJob } from '$lib/api/types';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import FrontHeader from '$lib/components/front/FrontHeader.svelte';
	import { absolute, bytes, clock, mediaLabel } from '$lib/format';

	const slug = $derived(page.params.slug ?? '');
	const id = $derived(page.params.id ?? '');

	let info = $state<FrontInfo | null>(null);
	let job = $state<FrontJob | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);

	const gallery = $derived(resolve('/(front)/f/[slug]', { slug }));

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [loadedInfo, loadedJob] = await Promise.all([front.info(slug), front.job(slug, id)]);
			if (current !== requestId) return;
			info = loadedInfo;
			job = loadedJob;
			document.title = `${loadedJob.title ?? mediaLabel(loadedJob.media)} · ${loadedInfo.name}`;
		} catch (err) {
			if (current !== requestId) return;
			if (err instanceof ApiError && err.unauthorized) {
				await goto(resolve('/(front)/f/[slug]/login', { slug }));
				return;
			}
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void slug;
		void id;
		void load();
	});

	async function logout() {
		try {
			await front.logout(slug);
		} finally {
			await goto(resolve('/(front)/f/[slug]/login', { slug }));
		}
	}
</script>

{#if info}
	<FrontHeader {info} onlogout={logout} />
{/if}

<main class="mx-auto max-w-5xl space-y-4 p-4">
	<a href={gallery} class="inline-flex items-center gap-1 anchor text-sm">
		<ArrowLeftIcon class="size-4" />
		{info?.name ?? 'Back'}
	</a>

	{#if error && !loading}
		<ErrorState {error} title="This media could not be opened" onretry={load} />
	{:else if !job || !info}
		<div class="aspect-video placeholder animate-pulse" aria-busy="true"></div>
	{:else}
		<section class="overflow-hidden card bg-surface-100-900" aria-label="Media">
			{#if job.media === 'video'}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video
					class="max-h-[80vh] w-full bg-black"
					controls
					autoplay
					playsinline
					preload="metadata"
					src={job.media_url}
					poster={job.thumbnail ?? undefined}
				></video>
			{:else if job.media === 'audio'}
				<div class="space-y-4 p-4">
					{#if job.thumbnail}
						<img src={job.thumbnail} alt="" class="mx-auto max-h-80 rounded-base" />
					{/if}
					<audio class="w-full" controls preload="metadata" src={job.media_url}></audio>
				</div>
			{:else if job.media === 'image'}
				<img
					class="max-h-[80vh] w-full object-contain"
					src={job.media_url}
					alt={job.title ?? 'Image'}
				/>
			{:else}
				<div class="flex items-center gap-4 p-6">
					<MediaKindIcon kind="file" class="size-10 text-surface-600-400" />
					<div>
						<p class="font-medium">{job.title ?? 'File'}</p>
						<p class="text-sm text-surface-600-400">{job.content_type} · {bytes(job.size)}</p>
					</div>
				</div>
			{/if}
		</section>

		<div class="flex flex-wrap items-start justify-between gap-4">
			<div class="min-w-0 space-y-1">
				<h1 class="h4 break-words">{job.title ?? mediaLabel(job.media)}</h1>
				<p class="text-sm text-surface-600-400">
					{[job.uploader, job.resolver].filter(Boolean).join(' · ')}
				</p>
			</div>
			<div class="flex flex-wrap gap-2">
				{#if job.webpage_url}
					<a href={job.webpage_url} class="btn preset-tonal" target="_blank" rel="noreferrer">
						<ExternalLinkIcon class="size-4" />
						Source
					</a>
				{/if}
				{#if info.downloads && job.download_url}
					<a href={job.download_url} class="btn preset-filled-primary-500" download>
						<DownloadIcon class="size-4" />
						Download
					</a>
				{/if}
			</div>
		</div>

		<section
			class="card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Details"
		>
			<KeyValue>
				<KeyValueRow
					label="Kind"
					value={job.live ? `${mediaLabel(job.media)} · recorded live` : mediaLabel(job.media)}
				/>
				<KeyValueRow
					label="Length"
					value={job.duration_secs === null ? null : clock(job.duration_secs)}
				/>
				<KeyValueRow label="Size" value={bytes(job.size)} />
				<KeyValueRow
					label="Dimensions"
					value={job.width && job.height ? `${job.width} × ${job.height}` : null}
				/>
				<KeyValueRow label="Format" value={job.content_type} />
				<KeyValueRow label="Published" value={absolute(job.published_at)} />
			</KeyValue>
		</section>
	{/if}
</main>
