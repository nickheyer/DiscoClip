<script lang="ts">
	import type { PageData } from './$types';
	import { MEDIA_LABELS, front } from '$lib/api';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatBytes, formatClock } from '$lib/format';
	import { MEDIA_ICONS } from '$lib/frontends';

	let { data }: { data: PageData } = $props();

	const job = $derived(data.job);
	const title = $derived(job.title ?? `${MEDIA_LABELS[job.media]} from ${job.resolver}`);
	const pageUrl = $derived(
		typeof window === 'undefined'
			? front.pagePath(data.slug, job.id)
			: `${window.location.origin}${front.pagePath(data.slug, job.id)}`
	);
</script>

<svelte:head>
	<title>{title} · {data.info.name}</title>
</svelte:head>

<div class="watch">
	<a class="back small" href={`/f/${data.slug}`}><Icon name="arrow-left" size={14} /> Back to {data.info.name}</a>

	<div class="stage">
		{#if job.media === 'video'}
			<!-- svelte-ignore a11y_media_has_caption -->
			<video class="player" controls autoplay playsinline preload="metadata" src={job.media_url} poster={job.thumbnail ?? undefined}></video>
		{:else if job.media === 'audio'}
			<div class="audio">
				{#if job.thumbnail}<img class="cover" src={job.thumbnail} alt="" />{:else}<span class="cover placeholder"><Icon name="volume" size={40} /></span>{/if}
				<audio class="audio-player" controls autoplay preload="metadata" src={job.media_url}></audio>
			</div>
		{:else if job.media === 'image'}
			<a href={job.media_url} target="_blank" rel="noreferrer"><img class="picture" src={job.media_url} alt={title} /></a>
		{:else}
			<div class="file">
				<span class="placeholder"><Icon name={MEDIA_ICONS.file} size={40} /></span>
				<span class="strong">{title}</span>
				<span class="muted small">{formatBytes(job.size)} · {job.content_type}</span>
				{#if job.download_url}
					<Button variant="primary" icon="download" href={job.download_url} external>Download</Button>
				{:else}
					<span class="faint small">This site does not hand files out.</span>
				{/if}
			</div>
		{/if}
	</div>

	<div class="meta">
		<div class="row-between wrap">
			<h1>{title}</h1>
			<div class="row">
				<CopyButton text={pageUrl} label="Copy link" />
				{#if job.download_url}
					<Button variant="secondary" icon="download" href={job.download_url} external>Download</Button>
				{/if}
			</div>
		</div>
		<div class="chips">
			<span class="chip"><Icon name={MEDIA_ICONS[job.media]} size={12} /> {MEDIA_LABELS[job.media]}</span>
			<span class="chip">{job.resolver}</span>
			{#if job.live}<Badge tone="danger" size="sm">Recorded stream</Badge>{/if}
		</div>
		<dl class="kv small">
			{#if job.uploader}<dt>By</dt><dd>{job.uploader}</dd>{/if}
			{#if job.webpage_url}<dt>Source</dt><dd><a href={job.webpage_url} target="_blank" rel="noreferrer" class="break">{job.webpage_url}</a></dd>{/if}
			<dt>Posted</dt><dd><Time value={job.published_at} mode="absolute" /></dd>
			{#if job.duration_secs != null}<dt>Length</dt><dd>{formatClock(job.duration_secs)}</dd>{/if}
			<dt>Size</dt><dd>{formatBytes(job.size)}{#if job.width && job.height} · {job.width}×{job.height}{/if}</dd>
		</dl>
	</div>
</div>

<style>
	.watch {
		display: flex;
		flex-direction: column;
		gap: 16px;
		max-width: 1100px;
		margin: 0 auto;
	}

	.back {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--text-2);
	}

	.stage {
		background: #000;
		border-radius: var(--radius);
		overflow: hidden;
		display: flex;
		align-items: center;
		justify-content: center;
		min-height: 240px;
	}

	.player {
		width: 100%;
		max-height: 78vh;
		display: block;
	}

	.picture {
		max-width: 100%;
		max-height: 80vh;
		display: block;
	}

	.audio,
	.file {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 16px;
		padding: 32px;
		width: 100%;
		box-sizing: border-box;
		color: #fff;
	}

	.cover {
		width: 240px;
		height: 240px;
		border-radius: var(--radius);
		object-fit: cover;
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--surface-3);
	}

	.audio-player {
		width: 100%;
		max-width: 640px;
	}

	.placeholder {
		color: var(--text-3);
	}

	.meta h1 {
		margin: 0;
		font-size: 20px;
	}

	.meta {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}

	.wrap {
		flex-wrap: wrap;
		gap: 10px;
	}
</style>
