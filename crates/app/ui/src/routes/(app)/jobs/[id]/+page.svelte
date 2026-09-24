<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import DownloadIcon from '@lucide/svelte/icons/download';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import XIcon from '@lucide/svelte/icons/x';
	import { Menu, Portal } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { jobs } from '$lib/api/endpoints';
	import type {
		Codec,
		Job,
		JobSummary,
		LogEntry,
		Progress,
		Stage,
		StageTiming,
		Variant
	} from '$lib/api/types';
	import Bytes from '$lib/components/Bytes.svelte';
	import CodeBlock from '$lib/components/CodeBlock.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import JobTitle from '$lib/components/JobTitle.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import { feed } from '$lib/events.svelte';
	import {
		absolute,
		bytes,
		clock,
		duration,
		EMPTY,
		mediaLabel,
		number,
		percent,
		span,
		stageLabel
	} from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const STAGES: Stage[] = ['resolve', 'download', 'transcode', 'publish', 'archive'];

	const id = $derived(page.params.id ?? '');

	let job = $state<Job | null>(null);
	let children = $state<JobSummary[]>([]);
	let progress = $state<{ stage: Stage; progress: Progress } | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let pending = $state<'retry' | 'cancel' | 'delete' | null>(null);
	let confirmDelete = $state(false);

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const loaded = await jobs.get(id);
			if (current !== requestId) return;
			job = loaded;
			children = loaded.artifacts.children.length > 0 ? await jobs.children(id) : [];
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void id;
		progress = null;
		void load();
	});

	onMount(() =>
		feed.onJob((event) => {
			if (event.job === id) {
				switch (event.kind) {
					case 'status':
						void load();
						break;
					case 'progress':
						progress = { stage: event.stage, progress: event.progress };
						break;
					case 'log':
						if (job) job = { ...job, log: [...job.log, event.entry] };
						break;
					case 'children':
						void load();
						break;
					case 'deleted':
						notify.info('This job was deleted');
						void goto(resolve('/jobs'));
						break;
				}
				return;
			}
			if (event.job_summary?.parent === id) {
				const summary = event.job_summary;
				const index = children.findIndex((child) => child.id === summary.id);
				if (index >= 0) {
					const next = children.slice();
					next[index] = summary;
					children = next;
				} else {
					children = [...children, summary];
				}
			}
		})
	);

	const summary = $derived.by((): JobSummary | null => {
		if (!job) return null;
		const resolved = job.artifacts.resolved;
		return {
			id: job.id,
			url: job.request.url,
			status: job.status,
			source: job.request.origin.source,
			origin: job.request.origin,
			destination: job.request.destination,
			submitted_by: job.request.submitted_by,
			parent: job.request.parent,
			retry_of: job.request.retry_of,
			title: resolved?.title ?? null,
			resolver: resolved?.resolver ?? null,
			media: job.artifacts.output?.info?.kind ?? resolved?.media ?? 'video',
			uploader: resolved?.uploader ?? null,
			webpage_url: resolved?.webpage_url ?? null,
			thumbnail: resolved?.thumbnail ?? null,
			duration_secs: resolved?.duration
				? resolved.duration.secs + resolved.duration.nanos / 1e9
				: null,
			live: resolved?.live ?? false,
			output_bytes: job.artifacts.output?.size ?? null,
			published_url: job.artifacts.published?.url ?? null,
			published_reference: job.artifacts.published?.reference ?? null,
			children: job.artifacts.children.length,
			archived_files: job.artifacts.archived?.files.length ?? 0,
			created_at: job.created_at,
			updated_at: job.updated_at,
			started_at: job.started_at,
			finished_at: job.finished_at
		};
	});

	const finished = $derived(
		job !== null &&
			(job.status.status === 'done' ||
				job.status.status === 'failed' ||
				job.status.status === 'cancelled')
	);
	const active = $derived(
		job !== null && (job.status.status === 'queued' || job.status.status === 'running')
	);
	const canManage = $derived(session.can('manage_jobs'));

	interface Step {
		stage: Stage;
		state: 'pending' | 'running' | 'done' | 'failed' | 'skipped';
		timing: StageTiming | null;
	}

	const steps = $derived.by((): Step[] => {
		if (!job) return [];
		const status = job.status;
		const timings = new Map(job.artifacts.timings.map((t) => [t.stage, t]));
		const failedAt = status.status === 'failed' ? status.stage : null;
		const runningAt = status.status === 'running' ? status.stage : null;
		const failedIndex = failedAt ? STAGES.indexOf(failedAt) : -1;
		return STAGES.map((stage, index) => {
			const timing = timings.get(stage) ?? null;
			let state: Step['state'] = 'pending';
			if (stage === failedAt) state = 'failed';
			else if (stage === runningAt) state = 'running';
			else if (timing?.ended_at) state = 'done';
			else if (failedIndex >= 0 && index > failedIndex) state = 'skipped';
			else if (status.status === 'cancelled' && !timing) state = 'skipped';
			else if (status.status === 'done' && !timing) state = 'skipped';
			return { stage, state, timing };
		});
	});

	function stepSeconds(step: Step): number | null {
		if (!step.timing) return null;
		const end = step.timing.ended_at ? Date.parse(step.timing.ended_at) : Date.now();
		return (end - Date.parse(step.timing.started_at)) / 1000;
	}

	const STEP_CLASS: Record<Step['state'], string> = {
		pending: 'preset-outlined-surface-300-700 text-surface-600-400',
		running: 'preset-filled-primary-500',
		done: 'preset-filled-success-500',
		failed: 'preset-filled-error-600-400',
		skipped: 'preset-tonal-surface line-through text-surface-600-400'
	};

	function codec(value: Codec | null | undefined): string {
		if (!value) return '';
		return typeof value === 'string' ? value : value.other;
	}

	async function retry() {
		pending = 'retry';
		try {
			const { id: fresh } = await jobs.retry(id);
			notify.success('Queued again');
			await goto(resolve('/(app)/jobs/[id]', { id: fresh }));
		} catch (err) {
			reportError(err, 'Could not retry');
		} finally {
			pending = null;
		}
	}

	async function cancel() {
		pending = 'cancel';
		try {
			await jobs.cancel(id);
			notify.success('Cancelled');
			await load();
		} catch (err) {
			reportError(err, 'Could not cancel');
		} finally {
			pending = null;
		}
	}

	async function remove() {
		await jobs.remove(id);
		notify.success('Deleted');
		await goto(resolve('/jobs'));
	}

	const variantColumns: Column<Variant>[] = [
		{ key: 'kind', label: 'Kind', value: (v) => v.kind },
		{ key: 'label', label: 'Label', value: (v) => v.label ?? v.format_id },
		{ key: 'container', label: 'Container', value: (v) => codec(v.container) },
		{
			key: 'codecs',
			label: 'Codecs',
			value: (v) => v.codecs ?? [codec(v.video), codec(v.audio)].filter(Boolean).join(' / ')
		},
		{
			key: 'size',
			label: 'Size',
			align: 'right',
			value: (v) => (v.size === null ? null : bytes(v.size))
		},
		{ key: 'height', label: 'Height', align: 'right', value: (v) => v.height },
		{ key: 'fps', label: 'FPS', align: 'right', value: (v) => v.fps },
		{
			key: 'bitrate',
			label: 'Bitrate',
			align: 'right',
			value: (v) => (v.bitrate === null ? null : `${Math.round(v.bitrate / 1000)} kbps`)
		},
		{ key: 'flags', label: 'Flags', cell: flagsCell }
	];

	const childColumns: Column<JobSummary>[] = [
		{ key: 'job', label: 'Entry', cell: childTitle },
		{ key: 'status', label: 'Status', cell: childStatus },
		{ key: 'size', label: 'Size', align: 'right', cell: childSize }
	];

	const outputInline = $derived(job ? jobs.downloadUrl(job.id, { inline: true }) : '');

	/** Everything the job has to hand over, as download menu entries. */
	const downloads = $derived.by((): { value: string; label: string; url: string }[] => {
		const current = job;
		if (!current) return [];
		const list: { value: string; label: string; url: string }[] = [];
		if (current.artifacts.output) {
			list.push({
				value: 'output',
				label: `Output · ${bytes(current.artifacts.output.size)}`,
				url: jobs.downloadUrl(current.id)
			});
		}
		if (current.artifacts.source) {
			list.push({
				value: 'source',
				label: `Source · ${bytes(current.artifacts.source.size)}`,
				url: jobs.downloadUrl(current.id, { artifact: 'source' })
			});
		}
		current.artifacts.subtitles.forEach((subtitle, index) => {
			list.push({
				value: `subtitle-${index}`,
				label: `Subtitles · ${subtitle.name ?? subtitle.language} (${subtitle.format})`,
				url: jobs.downloadUrl(current.id, { artifact: 'subtitle', index })
			});
		});
		return list;
	});

	/** Sends the browser to the file. The server answers as an attachment, so the page stays. */
	function download(value: string) {
		const item = downloads.find((entry) => entry.value === value);
		if (item) location.assign(item.url);
	}
</script>

{#snippet flagsCell(v: Variant)}
	<span class="text-surface-600-400" title={v.drm || undefined}>
		{[
			v.live ? 'live' : null,
			v.video_only ? 'video only' : null,
			v.audio_only ? 'audio only' : null,
			v.drm ? 'DRM' : null,
			v.cipher ? v.cipher.scheme : null,
			v.language || null
		]
			.filter((flag) => flag !== null)
			.join(' · ')}
	</span>
{/snippet}
{#snippet childTitle(child: JobSummary)}
	<JobTitle job={child} />
{/snippet}
{#snippet childStatus(child: JobSummary)}
	<Status job={child.status} />
{/snippet}
{#snippet childSize(child: JobSummary)}
	<Bytes value={child.output_bytes} />
{/snippet}

{#if error && !loading}
	<PageHeader title="Job" />
	<ErrorState {error} title="This job could not be loaded" onretry={load} />
{:else if !job || !summary}
	<PageHeader title="Job" />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-2/3 animate-pulse"></div>
		<div class="h-40 placeholder animate-pulse"></div>
	</div>
{:else}
	<a
		href={resolve('/jobs')}
		class="link-body inline-flex items-center gap-1 text-sm text-surface-700-300 hover:text-surface-950-50"
	>
		<ArrowLeftIcon class="size-4" />
		All jobs
	</a>

	<PageHeader title={summary.title ?? summary.url}>
		<div class="flex flex-wrap items-center gap-2 text-sm text-surface-600-400">
			<Status job={job.status} />
			<span class="inline-flex items-center gap-1">
				<MediaKindIcon kind={summary.media} />
				{mediaLabel(summary.media)}
			</span>
			{#if summary.resolver}<span>{summary.resolver}</span>{/if}
			{#if summary.live}<span class="text-error-700-300">Live</span>{/if}
			<span class="font-mono text-xs">{job.id}</span>
			<CopyButton text={job.id} label="Copy job id" />
		</div>
	</PageHeader>

	<Toolbar>
		{#if downloads.length > 0}
			<Menu onSelect={(details) => download(details.value)}>
				<Menu.Trigger class="btn preset-tonal">
					<DownloadIcon class="size-4" />
					Download
				</Menu.Trigger>
				<Portal>
					<Menu.Positioner>
						<Menu.Content class="min-w-56 card bg-surface-100-900 p-2 shadow-xl">
							{#each downloads as item (item.value)}
								<Menu.Item value={item.value}>
									<Menu.ItemText>{item.label}</Menu.ItemText>
								</Menu.Item>
							{/each}
						</Menu.Content>
					</Menu.Positioner>
				</Portal>
			</Menu>
		{/if}
		{#if canManage}
			{#if active}
				<button type="button" class="btn preset-tonal" onclick={cancel} disabled={pending !== null}>
					{#if pending === 'cancel'}<Spinner />{:else}<XIcon class="size-4" />{/if}
					Cancel
				</button>
			{/if}
			{#if finished}
				<button type="button" class="btn preset-tonal" onclick={retry} disabled={pending !== null}>
					{#if pending === 'retry'}<Spinner />{:else}<RotateCcwIcon class="size-4" />{/if}
					Retry
				</button>
				<button
					type="button"
					class="btn preset-tonal-error"
					onclick={() => (confirmDelete = true)}
					disabled={pending !== null}
				>
					<Trash2Icon class="size-4" />
					Delete
				</button>
			{/if}
		{/if}
	</Toolbar>

	{#if job.status.status === 'failed'}
		<div class="card preset-tonal-error p-4 text-sm" role="alert">
			<p class="font-semibold">Failed during {job.status.stage}</p>
			<p class="mt-1 break-words">{job.status.message}</p>
		</div>
	{/if}

	<section
		class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
		aria-label="Stages"
	>
		<ol class="grid gap-2 sm:grid-cols-5">
			{#each steps as step (step.stage)}
				{@const secs = stepSeconds(step)}
				<li class="space-y-1 rounded-base px-3 py-2 text-sm {STEP_CLASS[step.state]}">
					<p class="font-medium">{stageLabel(step.stage)}</p>
					<p class="text-sm opacity-80">
						{#if step.state === 'running'}
							{#if progress && progress.stage === step.stage}
								{#if progress.progress.total}
									{percent(progress.progress.done / progress.progress.total)}
								{:else}
									{number(progress.progress.done)} so far
								{/if}
							{:else}
								Running
							{/if}
						{:else if secs !== null}
							{span(secs)}
						{:else if step.state === 'skipped'}
							Skipped
						{:else}
							Waiting
						{/if}
					</p>
				</li>
			{/each}
		</ol>
		{#if progress && job.status.status === 'running' && progress.progress.total}
			<progress class="progress" value={progress.progress.done} max={progress.progress.total}
			></progress>
		{:else if job.status.status === 'running'}
			<progress class="progress"></progress>
		{/if}
	</section>

	{#if job.artifacts.output && job.artifacts.output.info}
		{@const info = job.artifacts.output.info}
		<section class="overflow-hidden card bg-surface-100-900" aria-label="Output">
			{#if info.kind === 'video'}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video class="max-h-[70vh] w-full bg-black" controls preload="metadata" src={outputInline}
				></video>
			{:else if info.kind === 'audio'}
				<div class="p-4">
					<audio class="w-full" controls preload="metadata" src={outputInline}></audio>
				</div>
			{:else if info.kind === 'image'}
				<img
					class="max-h-[70vh] w-full object-contain"
					src={outputInline}
					alt={summary.title ?? 'Output'}
				/>
			{:else}
				<div class="flex items-center gap-3 p-4 text-sm">
					<MediaKindIcon kind="file" class="size-6" />
					<span>{job.artifacts.output.path.split('/').pop()}</span>
					<span class="text-surface-600-400"><Bytes value={job.artifacts.output.size} /></span>
				</div>
			{/if}
		</section>
	{/if}

	<div class="grid gap-6 lg:grid-cols-2">
		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Request"
		>
			<h2 class="h6">Request</h2>
			<KeyValue>
				<KeyValueRow label="Link">
					<a href={job.request.url} class="link-body break-all" target="_blank" rel="noreferrer">
						{job.request.url}
						<ExternalLinkIcon class="inline size-3" />
					</a>
				</KeyValueRow>
				<KeyValueRow
					label="Origin"
					value="{job.request.origin.source} · {job.request.origin.reference}"
				/>
				{#if job.request.origin.url}
					<KeyValueRow label="Origin link">
						<a
							href={job.request.origin.url}
							class="link-body break-all"
							target="_blank"
							rel="noreferrer"
							>{job.request.origin.url} <ExternalLinkIcon class="inline size-3" /></a
						>
					</KeyValueRow>
				{/if}
				<KeyValueRow label="Destination" value={job.request.destination} />
				<KeyValueRow label="Submitted by" value={job.request.submitted_by} />
				<KeyValueRow label="Submitted"
					><RelativeTime at={job.created_at} />
					<span class="text-surface-600-400">({absolute(job.created_at)})</span></KeyValueRow
				>
				<KeyValueRow label="Started"><RelativeTime at={job.started_at} /></KeyValueRow>
				<KeyValueRow label="Finished"><RelativeTime at={job.finished_at} /></KeyValueRow>
				<KeyValueRow
					label="Max source"
					value={job.request.limits.max_source_bytes === null
						? 'Engine limit'
						: bytes(job.request.limits.max_source_bytes)}
				/>
				<KeyValueRow
					label="Max duration"
					value={job.request.limits.max_duration_secs === null
						? 'Engine limit'
						: clock(job.request.limits.max_duration_secs)}
				/>
				<KeyValueRow
					label="Max height"
					value={job.request.limits.max_height === null
						? 'Engine limit'
						: `${job.request.limits.max_height} px`}
				/>
				<KeyValueRow
					label="Clip"
					value={job.request.options.clip
						? `${duration(job.request.options.clip.start)} to ${job.request.options.clip.end ? duration(job.request.options.clip.end) : 'the end'}`
						: 'Whole'}
				/>
				<KeyValueRow
					label="Subtitles"
					value="{job.request.options.subtitles}{job.request.options.subtitle_language
						? ` · ${job.request.options.subtitle_language}`
						: ''}"
				/>
				{#if job.request.parent}
					<KeyValueRow label="Playlist">
						<a
							href={resolve('/(app)/jobs/[id]', { id: job.request.parent })}
							class="link-body font-mono text-xs">{job.request.parent}</a
						>
					</KeyValueRow>
				{/if}
				{#if job.request.retry_of}
					<KeyValueRow label="Retry of">
						<a
							href={resolve('/(app)/jobs/[id]', { id: job.request.retry_of })}
							class="link-body font-mono text-xs">{job.request.retry_of}</a
						>
					</KeyValueRow>
				{/if}
			</KeyValue>
		</section>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Result"
		>
			<h2 class="h6">Result</h2>
			<KeyValue>
				<KeyValueRow
					label="Delivery"
					value={job.artifacts.delivery === 'link' ? 'Link to a view page' : 'Upload'}
				/>
				{#if job.artifacts.link_reason}
					<KeyValueRow label="Linked because" value={job.artifacts.link_reason} />
				{/if}
				{#if job.artifacts.published}
					<KeyValueRow label="Published">
						{#if job.artifacts.published.url}
							<a
								href={job.artifacts.published.url}
								class="link-body break-all"
								target="_blank"
								rel="noreferrer"
								>{job.artifacts.published.reference} <ExternalLinkIcon class="inline size-3" /></a
							>
						{:else}
							{job.artifacts.published.reference}
						{/if}
						<span class="text-surface-600-400">
							· <RelativeTime at={job.artifacts.published.at} /></span
						>
					</KeyValueRow>
				{/if}
				{#if job.artifacts.archived}
					<KeyValueRow
						label="Archived"
						value="{number(job.artifacts.archived.files.length)} files · {bytes(
							job.artifacts.archived.bytes
						)}"
					/>
				{/if}
				{#if job.artifacts.output}
					{@const out = job.artifacts.output}
					<KeyValueRow label="Output" value="{out.path.split('/').pop()} · {bytes(out.size)}" />
					{#if out.info}
						<KeyValueRow
							label="Output format"
							value="{codec(out.info.container)}{out.info.video
								? ` · ${codec(out.info.video.codec)} ${out.info.video.width}×${out.info.video.height}${out.info.video.fps ? ` @ ${out.info.video.fps} fps` : ''}`
								: ''}{out.info.audio
								? ` · ${codec(out.info.audio.codec)} ${out.info.audio.channels}ch ${out.info.audio.sample_rate} Hz`
								: ''}"
						/>
						<KeyValueRow label="Output length" value={duration(out.info.duration)} />
					{/if}
				{/if}
				{#if job.artifacts.source}
					{@const src = job.artifacts.source}
					<KeyValueRow
						label="Source file"
						value="{src.path.split('/').pop()} · {bytes(src.size)}"
					/>
					{#if src.info}
						<KeyValueRow
							label="Source format"
							value="{codec(src.info.container)}{src.info.video
								? ` · ${codec(src.info.video.codec)} ${src.info.video.width}×${src.info.video.height}`
								: ''}{src.info.audio ? ` · ${codec(src.info.audio.codec)}` : ''}"
						/>
					{/if}
				{/if}
				{#if job.artifacts.subtitles.length > 0}
					<KeyValueRow
						label="Subtitles"
						value={job.artifacts.subtitles
							.map((s) => `${s.name ?? s.language} (${s.format})`)
							.join(', ')}
					/>
				{/if}
			</KeyValue>
		</section>
	</div>

	{#if job.artifacts.resolved}
		{@const resolved = job.artifacts.resolved}
		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label="Resolved"
		>
			<h2 class="h6">Resolved</h2>
			<KeyValue>
				<KeyValueRow label="Title" value={resolved.title} />
				<KeyValueRow label="Uploader">
					{#if resolved.uploader_url}
						<a href={resolved.uploader_url} class="link-body" target="_blank" rel="noreferrer"
							>{resolved.uploader ?? resolved.uploader_url}
							<ExternalLinkIcon class="inline size-3" /></a
						>
					{:else}
						{resolved.uploader ?? EMPTY}
					{/if}
				</KeyValueRow>
				<KeyValueRow label="Page">
					{#if resolved.webpage_url}
						<a
							href={resolved.webpage_url}
							class="link-body break-all"
							target="_blank"
							rel="noreferrer">{resolved.webpage_url} <ExternalLinkIcon class="inline size-3" /></a
						>
					{:else}
						<span class="text-surface-600-400">{EMPTY}</span>
					{/if}
				</KeyValueRow>
				<KeyValueRow
					label="Uploaded"
					value={resolved.uploaded_at ? absolute(resolved.uploaded_at) : null}
				/>
				<KeyValueRow label="Length" value={duration(resolved.duration)} />
				<KeyValueRow label="Age limit" value={resolved.age_limit} />
				<KeyValueRow
					label="Subtitle tracks"
					value={resolved.subtitles.length > 0
						? resolved.subtitles.map((s) => `${s.language}${s.auto ? ' (auto)' : ''}`).join(', ')
						: null}
				/>
			</KeyValue>
			{#if resolved.description}
				<details class="text-sm">
					<summary class="cursor-pointer text-surface-600-400">Description</summary>
					<p class="mt-2 whitespace-pre-wrap">{resolved.description}</p>
				</details>
			{/if}
			{#if resolved.variants.length > 0}
				<h3 class="text-sm font-semibold">Variants</h3>
				<DataTable rows={resolved.variants} columns={variantColumns} rowKey={(v) => v.url} dense />
			{/if}
		</section>
	{/if}

	{#if job.artifacts.children.length > 0}
		<section class="space-y-3" aria-label="Playlist entries">
			<h2 class="h6">Playlist entries ({number(job.artifacts.children.length)})</h2>
			<DataTable
				rows={children}
				columns={childColumns}
				rowKey={(c) => c.id}
				rowHref={(c) => resolve('/(app)/jobs/[id]', { id: c.id })}
				rowLabel="View"
			/>
		</section>
	{/if}

	<section
		class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
		aria-label="Log"
	>
		<h2 class="h6">Log</h2>
		{#if job.log.length === 0}
			<p class="text-sm text-surface-600-400">Nothing logged yet.</p>
		{:else}
			<CodeBlock
				wrap
				code={job.log
					.map(
						(entry: LogEntry) =>
							`${absolute(entry.at)}  ${(entry.stage ?? '').padEnd(9)} ${entry.message}`
					)
					.join('\n')}
			/>
		{/if}
	</section>
{/if}

<Confirm
	bind:open={confirmDelete}
	title="Delete this job?"
	message="Its record and cached files go away."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
