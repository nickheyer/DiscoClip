<script lang="ts">
	import Field from '$lib/components/Field.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import type { PageData } from './$types';
	import { MEDIA_KINDS, MEDIA_LABELS, front, isApiError, messageOf } from '$lib/api';
	import type { FrontJob, MediaKind } from '$lib/api';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatClock } from '$lib/format';
	import { MEDIA_ICONS } from '$lib/frontends';

	let { data }: { data: PageData } = $props();

	const info = $derived(data.info);
	let jobs = $state<FrontJob[]>([]);
	let next = $state<string | null>(null);
	$effect(() => {
		jobs = data.page.jobs;
		next = data.page.next;
	});

	let q = $state('');
	$effect(() => {
		q = data.query.q ?? '';
	});
	const media = $derived(data.query.media ?? null);
	const platform = $derived(data.query.resolver ?? null);

	function navigate(changes: { q?: string; media?: MediaKind | null; platform?: string | null }) {
		const params = new URLSearchParams();
		const nextQ = changes.q !== undefined ? changes.q : q;
		const nextMedia = changes.media !== undefined ? changes.media : media;
		const nextPlatform = changes.platform !== undefined ? changes.platform : platform;
		if (nextQ.trim()) params.set('q', nextQ.trim());
		if (nextMedia) params.set('media', nextMedia);
		if (nextPlatform) params.set('platform', nextPlatform);
		const search = params.toString();
		void goto(`/f/${data.slug}${search ? `?${search}` : ''}`, { keepFocus: true, noScroll: true });
	}

	function search(event: SubmitEvent) {
		event.preventDefault();
		navigate({ q });
	}

	let loadingMore = $state(false);
	let moreError = $state<string | null>(null);

	async function loadMore() {
		if (!next) return;
		loadingMore = true;
		moreError = null;
		try {
			const more = await front.jobs(data.slug, { ...data.query, before: next });
			jobs = [...jobs, ...more.jobs];
			next = more.next;
		} catch (cause) {
			if (isApiError(cause) && cause.status === 401) {
				void goto(`/f/${data.slug}/login?next=${encodeURIComponent(`${page.url.pathname}${page.url.search}`)}`);
				return;
			}
			moreError = messageOf(cause);
		} finally {
			loadingMore = false;
		}
	}

	function kindOf(job: FrontJob): string {
		return MEDIA_LABELS[job.media];
	}
</script>

<div class="stack-lg">
	<section class="head">
		<h1>{info.name}</h1>
		{#if info.description}<p class="muted">{info.description}</p>{/if}
	</section>

	<section class="filters">
		<form class="search" onsubmit={search}>
			<Field label="Search" for="control-2676"><input id="control-2676" class="input" type="search" placeholder="Search titles" bind:value={q} aria-label="Search" /></Field>
			<Button type="submit" variant="secondary" icon="search">Search</Button>
		</form>
		<div class="chips" role="group" aria-label="Kind of media">
			<button type="button" class={['chip', 'pick', !media && 'on']} onclick={() => navigate({ media: null })}>All</button>
			{#each MEDIA_KINDS as kind (kind)}
				<button type="button" class={['chip', 'pick', media === kind && 'on']} onclick={() => navigate({ media: kind })}>
					<Icon name={MEDIA_ICONS[kind]} size={12} /> {MEDIA_LABELS[kind]}
				</button>
			{/each}
		</div>
		{#if info.platforms.length > 1}
			<Field label="Platform" for="control-3342"><select id="control-3342" class="select platform" value={platform ?? ''} onchange={(e) => navigate({ platform: (e.currentTarget as HTMLSelectElement).value || null })} aria-label="Platform">
				<option value="">Every platform</option>
				{#each info.platforms as id (id)}
					<option value={id}>{id}</option>
				{/each}
			</select></Field>
		{/if}
	</section>

	{#if jobs.length === 0}
		<Empty icon="video" title="Nothing here yet" description={q || media || platform ? 'Nothing matches these filters.' : 'No media has been posted here yet.'} />
	{:else}
		<div class="grid">
			{#each jobs as job (job.id)}
				<a class="card tile" href={`/f/${data.slug}/j/${job.id}`}>
					<div class="thumb">
						{#if job.thumbnail}
							<img src={job.thumbnail} alt="" loading="lazy" />
						{:else if job.media === 'image'}
							<img src={job.media_url} alt="" loading="lazy" />
						{:else}
							<span class="placeholder"><Icon name={MEDIA_ICONS[job.media]} size={28} /></span>
						{/if}
						{#if job.duration_secs != null}<span class="length">{formatClock(job.duration_secs)}</span>{/if}
						{#if job.live}<span class="stream"><Badge tone="danger" size="sm">Recorded stream</Badge></span>{/if}
					</div>
					<div class="text">
						<span class="title">{job.title ?? `${kindOf(job)} from ${job.resolver}`}</span>
						<span class="faint small">
							{#if job.uploader}{job.uploader} · {/if}{kindOf(job)} · <Time value={job.published_at} />
						</span>
					</div>
				</a>
			{/each}
		</div>
		{#if moreError}
			<p class="error-text small">{moreError}</p>
		{/if}
		{#if next}
			<div class="more">
				<Button variant="secondary" loading={loadingMore} onclick={loadMore}>Load more</Button>
			</div>
		{/if}
	{/if}
</div>

<style>
	.head h1 {
		margin: 0 0 4px;
		font-size: 24px;
	}

	.head p {
		margin: 0;
	}

	.filters {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px;
	}

	.search {
		display: flex;
		gap: 8px;
		flex: 1 1 280px;
	}

	.search .input {
		flex: 1;
	}

	.platform {
		max-width: 220px;
	}

	.pick {
		cursor: pointer;
		border: 1px solid var(--border);
		background: var(--surface);
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}

	.pick.on {
		background: var(--accent-soft);
		color: var(--accent-text);
		border-color: transparent;
	}

	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(230px, 1fr));
		gap: 16px;
	}

	.tile {
		display: flex;
		flex-direction: column;
		overflow: hidden;
		text-decoration: none;
		color: inherit;
		transition: transform 0.12s, box-shadow 0.12s;
	}

	.tile:hover {
		text-decoration: none;
		transform: translateY(-2px);
		box-shadow: var(--shadow);
	}

	.thumb {
		position: relative;
		aspect-ratio: 16 / 9;
		background: #000;
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.thumb img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}

	.placeholder {
		color: var(--text-3);
	}

	.length {
		position: absolute;
		right: 8px;
		bottom: 8px;
		padding: 1px 6px;
		border-radius: 4px;
		background: rgb(0 0 0 / 0.75);
		color: #fff;
		font-size: 13px;
		font-variant-numeric: tabular-nums;
	}

	.stream {
		position: absolute;
		left: 8px;
		top: 8px;
	}

	.text {
		display: flex;
		flex-direction: column;
		gap: 3px;
		padding: 10px 12px 12px;
	}

	.title {
		font-weight: 600;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
	}

	.more {
		display: flex;
		justify-content: center;
	}
</style>
