<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import DownloadIcon from '@lucide/svelte/icons/download';
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import SquareIcon from '@lucide/svelte/icons/square';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import XIcon from '@lucide/svelte/icons/x';
	import { Accordion, Menu, Portal, Progress, Steps } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { MediaQuery } from 'svelte/reactivity';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { jobs } from '$lib/api/endpoints';
	import type {
		Codec,
		Job,
		JobSummary,
		LogEntry,
		Progress as JobProgress,
		Stage,
		SubtitleMode,
		Variant,
		VideoTrack
	} from '$lib/api/types';
	import Bytes from '$lib/components/Bytes.svelte';
	import Card from '$lib/components/Card.svelte';
	import CodeBlock from '$lib/components/CodeBlock.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Identifier from '$lib/components/Identifier.svelte';
	import JobTitle from '$lib/components/JobTitle.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PlaceLine from '$lib/components/PlaceLine.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { feed } from '$lib/events.svelte';
	import {
		absolute,
		bytes,
		durationSecs,
		mediaLabel,
		number,
		percent,
		stageLabel
	} from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const STAGES: Stage[] = ['resolve', 'download', 'transcode', 'publish', 'archive'];

	/** Wide enough for the five stages in a row; below it they stack. */
	const wide = new MediaQuery('(min-width: 40rem)');

	const SUBTITLES: Record<SubtitleMode, string> = {
		keep: 'Kept as tracks',
		burn: 'Burnt into the picture',
		skip: 'Skipped'
	};

	type StepState = 'pending' | 'running' | 'done' | 'failed' | 'skipped';

	interface Step {
		stage: Stage;
		state: StepState;
		/** What the step's second line reads when it is not a time: a percentage, "Skipped". */
		detail: string;
		/** How long the stage took, in seconds, once it has ended. */
		secs: number | null;
		/** A live capture under way: seconds captured of the cap, and bytes recorded. */
		capture: { done: number; total: number | null; bytes: number } | null;
	}

	/** How each stage's indicator is coloured. */
	const INDICATOR: Record<StepState, string> = {
		pending: 'preset-outlined-surface-300-700',
		running: 'preset-filled-primary-500 animate-pulse',
		done: 'preset-filled-success-500',
		failed: 'preset-filled-error-500',
		skipped: 'preset-tonal opacity-60'
	};
	const STEP_TEXT: Record<StepState, string> = {
		pending: 'text-surface-600-400',
		running: 'text-primary-600-400',
		done: '',
		failed: 'text-error-600-400',
		skipped: 'text-surface-600-400 line-through'
	};

	const id = $derived(page.params.id ?? '');

	let job = $state<Job | null>(null);
	let children = $state<JobSummary[]>([]);
	let progress = $state<{ stage: Stage; progress: JobProgress } | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let pending = $state<'retry' | 'cancel' | 'stop' | 'delete' | null>(null);
	let confirmDelete = $state(false);
	let confirmCancel = $state(false);

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
					case 'recording':
					case 'stop':
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

	const resolved = $derived(job?.artifacts.resolved ?? null);
	const title = $derived(resolved?.title ?? job?.request.url ?? 'Job');
	const media = $derived(job?.artifacts.output?.info?.kind ?? resolved?.media ?? 'video');

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
	/** A live capture is under way: the recording grows and can be stopped or watched. */
	const capturing = $derived(
		job !== null &&
			job.artifacts.recording !== null &&
			job.status.status === 'running' &&
			job.status.stage === 'download'
	);
	/** When the capture began, for the elapsed time that ticks beside the player. */
	const captureStart = $derived(
		job?.artifacts.timings.find((t) => t.stage === 'download')?.started_at ?? null
	);
	const recordingUrl = $derived(
		job ? jobs.downloadUrl(job.id, { artifact: 'recording', inline: true }) : ''
	);

	/** What each stage's line reads while it runs. */
	function runningDetail(stage: Stage): string {
		if (progress && progress.stage === stage) {
			return progress.progress.total
				? percent(progress.progress.done / progress.progress.total)
				: `${number(progress.progress.done)} so far`;
		}
		return 'Running';
	}

	/** The live capture under way at `stage`, when the progress says it is one. */
	function runningCapture(stage: Stage): Step['capture'] {
		if (progress && progress.stage === stage && progress.progress.bytes !== null) {
			return {
				done: progress.progress.done,
				total: progress.progress.total,
				bytes: progress.progress.bytes
			};
		}
		return null;
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
			const secs = timing
				? ((timing.ended_at ? Date.parse(timing.ended_at) : Date.now()) -
						Date.parse(timing.started_at)) /
					1000
				: null;
			let state: StepState = 'pending';
			if (stage === failedAt) state = 'failed';
			else if (stage === runningAt) state = 'running';
			else if (timing?.ended_at) state = 'done';
			else if (failedIndex >= 0 && index > failedIndex) state = 'skipped';
			else if (status.status === 'cancelled' && !timing) state = 'skipped';
			else if (status.status === 'done' && !timing) state = 'skipped';
			const capture = state === 'running' ? runningCapture(stage) : null;
			const detail =
				state === 'running'
					? capture
						? ''
						: runningDetail(stage)
					: state === 'skipped'
						? 'Skipped'
						: state === 'pending'
							? 'Waiting'
							: '';
			const took = state === 'done' || state === 'failed' ? secs : null;
			return { stage, state, detail, secs: took, capture };
		});
	});

	/** Which step the stepper stands on: the one running or failed, past the end once done. */
	const currentStep = $derived.by((): number => {
		if (!job) return 0;
		if (job.status.status === 'done') return STAGES.length;
		if (job.status.status === 'running' || job.status.status === 'failed') {
			return STAGES.indexOf(job.status.stage);
		}
		const firstOpen = steps.findIndex((step) => step.state !== 'done');
		return firstOpen === -1 ? STAGES.length : firstOpen;
	});

	function codec(value: Codec | null | undefined): string {
		if (!value) return '';
		return typeof value === 'string' ? value : value.other;
	}

	/** What a picture needed on its way out, as words after its size. */
	function pictureFlags(track: VideoTrack): string {
		const flags: string[] = [];
		if (track.hdr) {
			flags.push(
				track.hdr.format === 'pq'
					? 'HDR10'
					: track.hdr.format === 'hlg'
						? 'HLG'
						: `Dolby Vision ${track.hdr.profile}`
			);
		}
		if (track.field_order === 'top_first' || track.field_order === 'bottom_first') {
			flags.push('interlaced');
		}
		if (track.projection) flags.push('360°');
		if (track.stereo) flags.push('3D');
		if (track.vfr) flags.push('variable rate');
		if (track.alpha) flags.push('transparent');
		if (track.sample_aspect)
			flags.push(`${track.sample_aspect[0]}:${track.sample_aspect[1]} pixels`);
		return flags.length > 0 ? ` · ${flags.join(' · ')}` : '';
	}

	function variantFlags(v: Variant): string {
		return [
			v.live ? 'live' : null,
			v.video_only ? 'video only' : null,
			v.audio_only ? 'audio only' : null,
			v.drm ? 'DRM' : null,
			v.cipher ? v.cipher.scheme : null,
			v.language || null,
			v.audio_track || null,
			v.audio_default ? 'default' : null,
			v.audio_dubbed ? 'dub' : null
		]
			.filter((flag) => flag !== null)
			.join(' · ');
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

	/** Cancelling a live capture deletes what was recorded, so it is confirmed first. */
	function askCancel() {
		if (capturing) confirmCancel = true;
		else void cancel();
	}

	async function stop() {
		pending = 'stop';
		try {
			await jobs.stop(id);
			notify.success('Stopped');
			await load();
		} catch (err) {
			reportError(err, 'Could not stop');
		} finally {
			pending = null;
		}
	}

	/** The recorded time a cancel would delete, in seconds. */
	const recordedSecs = $derived(
		captureStart ? Math.max(0, (Date.now() - Date.parse(captureStart)) / 1000) : 0
	);

	async function remove() {
		await jobs.remove(id);
		notify.success('Deleted');
		await goto(resolve('/jobs'));
	}

	const variantColumns: Column<Variant>[] = [
		{ key: 'kind', label: 'Kind', value: (v) => v.kind },
		{ key: 'label', label: 'Label', value: (v) => v.label ?? v.format_id },
		{ key: 'container', label: 'Container', value: (v) => codec(v.container), optional: true },
		{
			key: 'codecs',
			label: 'Codecs',
			value: (v) => v.codecs ?? [codec(v.video), codec(v.audio)].filter(Boolean).join(' / '),
			optional: true
		},
		{
			key: 'size',
			label: 'Size',
			align: 'right',
			value: (v) => (v.size === null ? null : bytes(v.size)),
			optional: true
		},
		{
			key: 'height',
			label: 'Height',
			align: 'right',
			value: (v) => (v.height === null ? null : `${v.height} px`),
			optional: true
		},
		{ key: 'fps', label: 'FPS', align: 'right', value: (v) => v.fps, optional: true },
		{
			key: 'bitrate',
			label: 'Bitrate',
			align: 'right',
			value: (v) => (v.bitrate === null ? null : `${Math.round(v.bitrate / 1000)} kbps`),
			optional: true
		},
		{
			key: 'flags',
			label: 'Flags',
			value: variantFlags,
			class: 'text-surface-600-400',
			optional: true
		}
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

	const back = { href: resolve('/jobs'), label: 'Jobs' };
</script>

{#snippet childTitle(child: JobSummary)}
	<JobTitle job={child} />
{/snippet}
{#snippet childStatus(child: JobSummary)}
	<Status job={child.status} />
{/snippet}
{#snippet childSize(child: JobSummary)}
	<Bytes value={child.output_bytes} />
{/snippet}

{#snippet external(href: string, text: string)}
	<a
		{href}
		class="inline-flex max-w-full items-center gap-1 anchor"
		target="_blank"
		rel="noreferrer"
	>
		<span class="truncate">{text}</span>
		<ExternalLinkIcon class="size-3 shrink-0" />
	</a>
{/snippet}

{#if error && !loading}
	<PageHeader title="Job" {back} />
	<ErrorState {error} title="This job could not be loaded" onretry={load} />
{:else if !job}
	<PageHeader title="Job" {back} />
	<div class="space-y-4" aria-busy="true">
		<div class="h-8 placeholder w-2/3 animate-pulse"></div>
		<div class="h-32 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader {title} {back}>
		<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-surface-600-400">
			<Status job={job.status} />
			{#if job.status.status === 'failed'}
				<span class="min-w-0 break-words text-error-600-400" role="alert">{job.status.message}</span
				>
			{/if}
			<span class="inline-flex items-center gap-1">
				<MediaKindIcon kind={media} class="size-4" />
				{mediaLabel(media)}
			</span>
			{#if resolved?.resolver}<span>{resolved.resolver}</span>{/if}
			{#if resolved?.uploader_url}
				{@render external(resolved.uploader_url, resolved.uploader ?? resolved.uploader_url)}
			{:else if resolved?.uploader}
				<span>{resolved.uploader}</span>
			{/if}
			{#if resolved?.duration}<Duration value={durationSecs(resolved.duration)} />{/if}
			{#if resolved?.live}<span class="text-error-600-400">Live</span>{/if}
			<Identifier value={job.id} label="Copy job id" />
		</div>
		{#snippet actions()}
			{#if downloads.length > 0}
				<Menu onSelect={(details) => download(details.value)}>
					<Menu.Trigger class="btn preset-tonal">
						<DownloadIcon class="size-4" />
						Download
					</Menu.Trigger>
					<Portal>
						<Menu.Positioner class="z-40">
							<Menu.Content>
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
				{#if capturing}
					<button type="button" class="btn preset-tonal" onclick={stop} disabled={pending !== null}>
						{#if pending === 'stop'}<Spinner />{:else}<SquareIcon class="size-4" />{/if}
						Stop
					</button>
				{/if}
				{#if active}
					<button
						type="button"
						class="btn preset-tonal"
						onclick={askCancel}
						disabled={pending !== null}
					>
						{#if pending === 'cancel'}<Spinner />{:else}<XIcon class="size-4" />{/if}
						Cancel
					</button>
				{/if}
				{#if finished}
					<button
						type="button"
						class="btn preset-tonal-error"
						onclick={() => (confirmDelete = true)}
						disabled={pending !== null}
					>
						<Trash2Icon class="size-4" />
						Delete
					</button>
					<button
						type="button"
						class="btn preset-filled-primary-500"
						onclick={retry}
						disabled={pending !== null}
					>
						{#if pending === 'retry'}<Spinner />{:else}<RotateCcwIcon class="size-4" />{/if}
						Retry
					</button>
				{/if}
			{/if}
		{/snippet}
	</PageHeader>

	<Card label="Stages">
		<div class="space-y-4">
			<!-- Skeleton's Steps: one step per pipeline stage, coloured by how it went. -->
			<Steps
				count={STAGES.length}
				step={currentStep}
				orientation={wide.current ? 'horizontal' : 'vertical'}
				class="w-full"
			>
				<Steps.List>
					{#each steps as step, index (step.stage)}
						<Steps.Item {index}>
							<Steps.Trigger class="pointer-events-none">
								<Steps.Indicator class={INDICATOR[step.state]}>
									{#if step.state === 'done'}
										<CheckIcon class="size-4" />
									{:else if step.state === 'failed'}
										<XIcon class="size-4" />
									{:else}
										{index + 1}
									{/if}
								</Steps.Indicator>
								<span class="flex flex-col items-start text-sm">
									<span class="font-medium {STEP_TEXT[step.state]}">{stageLabel(step.stage)}</span>
									{#if step.capture}
										<span class="text-xs text-surface-600-400 tabular-nums">
											<Duration value={step.capture.done} />
											{#if step.capture.total !== null}
												of <Duration value={step.capture.total} />
											{/if}
											· <Bytes value={step.capture.bytes} />
										</span>
									{:else if step.secs !== null}
										<Duration value={step.secs} class="text-xs text-surface-600-400" />
									{:else}
										<span class="text-xs text-surface-600-400 tabular-nums">{step.detail}</span>
									{/if}
								</span>
							</Steps.Trigger>
							{#if index < steps.length - 1}
								<Steps.Separator />
							{/if}
						</Steps.Item>
					{/each}
				</Steps.List>
			</Steps>
			{#if job.status.status === 'running'}
				<Progress
					value={progress && progress.progress.total ? progress.progress.done : null}
					max={progress?.progress.total ?? 100}
					aria-label="Job progress"
				>
					<Progress.Track class="h-1.5">
						<Progress.Range class="bg-primary-500" />
					</Progress.Track>
				</Progress>
			{/if}
		</div>
	</Card>

	{#if capturing}
		<section class="overflow-hidden card preset-filled-surface-100-900" aria-label="Recording">
			<video
				class="max-h-[60vh] w-full bg-black"
				controls
				autoplay
				muted
				playsinline
				src={recordingUrl}
			></video>
			<div class="flex flex-wrap items-center gap-3 p-3 text-sm">
				<Status label="Live" tone="error" pulse />
				<Duration since={captureStart} />
			</div>
		</section>
	{:else if job.artifacts.output && job.artifacts.output.info}
		{@const info = job.artifacts.output.info}
		<section class="overflow-hidden card preset-filled-surface-100-900" aria-label="Output">
			{#if info.kind === 'video'}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video class="max-h-[60vh] w-full bg-black" controls preload="metadata" src={outputInline}
				></video>
			{:else if info.kind === 'audio'}
				<div class="p-4">
					<audio class="w-full" controls preload="metadata" src={outputInline}></audio>
				</div>
			{:else if info.kind === 'image'}
				<img
					class="max-h-[60vh] w-full object-contain"
					src={outputInline}
					alt={resolved?.title ?? 'Output'}
				/>
			{:else}
				<div class="flex items-center gap-3 p-4 text-sm">
					<MediaKindIcon kind="file" class="size-5" />
					<span>{job.artifacts.output.path.split('/').pop()}</span>
					<span class="text-surface-600-400"><Bytes value={job.artifacts.output.size} /></span>
				</div>
			{/if}
		</section>
	{/if}

	<div class="grid items-start gap-6 lg:grid-cols-2">
		<Card title="Request">
			<KeyValue>
				<KeyValueRow label="Link">
					{@render external(job.request.url, job.request.url)}
				</KeyValueRow>
				<KeyValueRow label="From">
					<span class="flex flex-wrap items-center gap-x-3 gap-y-1">
						<PlaceLine
							origin={job.request.origin}
							place={job.place}
							destination={job.request.destination}
							submittedBy={job.request.submitted_by}
						/>
						{#if job.request.origin.url}
							{@render external(job.request.origin.url, 'Open message')}
						{/if}
					</span>
				</KeyValueRow>
				<KeyValueRow label="Submitted"><Timestamp at={job.created_at} /></KeyValueRow>
				{#if job.started_at}
					<KeyValueRow label="Started"><Timestamp at={job.started_at} /></KeyValueRow>
				{/if}
				{#if job.finished_at}
					<KeyValueRow label="Finished"><Timestamp at={job.finished_at} /></KeyValueRow>
				{/if}
				{#if job.started_at}
					<KeyValueRow label="Elapsed">
						<Duration since={job.started_at} until={job.finished_at} />
					</KeyValueRow>
				{/if}
				<KeyValueRow label="Max source size">
					<Bytes value={job.limits_in_force.max_source_bytes} />
				</KeyValueRow>
				{#if job.limits_in_force.max_duration_secs === 0}
					<KeyValueRow label="Max duration"><Status label="No live" tone="warning" /></KeyValueRow>
				{:else if job.limits_in_force.max_duration_secs !== null}
					<KeyValueRow label="Max duration">
						<Duration value={job.limits_in_force.max_duration_secs} />
					</KeyValueRow>
				{/if}
				<KeyValueRow label="Max height" value="{number(job.limits_in_force.max_height)} px" />
				{#if resolved?.live}
					<KeyValueRow label="Max capture">
						<Duration value={job.limits_in_force.max_capture_secs} />
					</KeyValueRow>
				{/if}
				{#if job.request.options.clip}
					{@const clip = job.request.options.clip}
					<KeyValueRow label="Clip">
						<span class="inline-flex items-center gap-1">
							<Duration value={durationSecs(clip.start)} />
							{#if clip.end}
								<span class="text-surface-600-400">to</span>
								<Duration value={durationSecs(clip.end)} />
							{/if}
						</span>
					</KeyValueRow>
				{/if}
				<KeyValueRow
					label="Subtitles"
					value="{SUBTITLES[job.request.options.subtitles]}{job.request.options.subtitle_language
						? ` · ${job.request.options.subtitle_language}`
						: ''}"
				/>
				<KeyValueRow label="Audio language" value={job.request.options.audio_language} />
				{#if job.request.parent}
					<KeyValueRow label="Playlist">
						<Identifier
							value={job.request.parent}
							href={resolve('/(app)/jobs/[id]', { id: job.request.parent })}
							label="Copy playlist job id"
						/>
					</KeyValueRow>
				{/if}
				{#if job.request.retry_of}
					<KeyValueRow label="Retry of">
						<Identifier
							value={job.request.retry_of}
							href={resolve('/(app)/jobs/[id]', { id: job.request.retry_of })}
							label="Copy the earlier job’s id"
						/>
					</KeyValueRow>
				{/if}
			</KeyValue>
		</Card>

		<Card title="Result">
			<KeyValue>
				{#if job.artifacts.published}
					<KeyValueRow label="Posted">
						{#if job.artifacts.published.url}
							{@render external(
								job.artifacts.published.url,
								job.artifacts.delivery === 'link' ? 'A link to the view page' : 'The file'
							)}
						{:else}
							{job.artifacts.delivery === 'link' ? 'A link to the view page' : 'The file'}
						{/if}
						<span class="text-surface-600-400">
							· <Timestamp at={job.artifacts.published.at} /></span
						>
					</KeyValueRow>
				{/if}
				{#if job.artifacts.link_reason}
					<KeyValueRow label="Linked because" value={job.artifacts.link_reason} />
				{/if}
				{#if job.artifacts.output}
					{@const out = job.artifacts.output}
					<KeyValueRow label="Output">
						{out.path.split('/').pop()}
						<span class="text-surface-600-400">
							· <Bytes value={out.size} />{#if out.info?.duration}
								· <Duration value={durationSecs(out.info.duration)} />{/if}
						</span>
					</KeyValueRow>
					{#if out.info}
						<KeyValueRow
							label="Format"
							value="{codec(out.info.container)}{out.info.video
								? ` · ${codec(out.info.video.codec)} ${out.info.video.width}×${out.info.video.height}${out.info.video.fps ? ` at ${out.info.video.fps} fps` : ''}`
								: ''}{out.info.audio
								? ` · ${codec(out.info.audio.codec)} ${out.info.audio.channels} ch ${(out.info.audio.sample_rate / 1000).toLocaleString()} kHz`
								: ''}"
						/>
					{/if}
				{/if}
				{#if job.artifacts.source}
					{@const src = job.artifacts.source}
					<KeyValueRow label="Source">
						{src.path.split('/').pop()}
						<span class="text-surface-600-400">
							· {bytes(src.size)}{src.info
								? ` · ${codec(src.info.container)}${src.info.video ? ` · ${codec(src.info.video.codec)} ${src.info.video.width}×${src.info.video.height}${pictureFlags(src.info.video)}` : ''}${src.info.audio ? ` · ${codec(src.info.audio.codec)}${src.info.audio.language ? ` ${src.info.audio.language}` : ''}` : ''}${src.info.subtitles.length > 0 ? ` · ${src.info.subtitles.length} subtitle ${src.info.subtitles.length === 1 ? 'stream' : 'streams'}` : ''}`
								: ''}
						</span>
					</KeyValueRow>
				{/if}
				{#if job.artifacts.subtitles.length > 0}
					<KeyValueRow
						label="Subtitles"
						value={job.artifacts.subtitles
							.map((s) => `${s.name ?? s.language} (${s.format})`)
							.join(', ')}
					/>
				{/if}
				{#if job.artifacts.archived}
					<KeyValueRow
						label="Archived"
						value="{number(job.artifacts.archived.files.length)} files · {bytes(
							job.artifacts.archived.bytes
						)}"
					/>
				{/if}
			</KeyValue>
		</Card>
	</div>

	{#if resolved}
		<Card title="Resolved" description="What {resolved.resolver} said about the link.">
			<div class="space-y-4">
				<KeyValue>
					{#if resolved.webpage_url}
						<KeyValueRow label="Page">
							{@render external(resolved.webpage_url, resolved.webpage_url)}
						</KeyValueRow>
					{/if}
					{#if resolved.uploaded_at}
						<KeyValueRow label="Uploaded" value={absolute(resolved.uploaded_at)} />
					{/if}
					{#if resolved.age_limit}
						<KeyValueRow label="Age limit" value="{resolved.age_limit}+" />
					{/if}
					{#if resolved.subtitles.length > 0}
						<KeyValueRow
							label="Subtitle tracks"
							value={resolved.subtitles
								.map((s) => `${s.language}${s.auto ? ' (auto)' : ''}`)
								.join(', ')}
						/>
					{/if}
				</KeyValue>
				{#if resolved.description || resolved.variants.length > 0}
					<!-- Skeleton's Accordion folds the long parts away: the description and the variants offered. -->
					<Accordion
						multiple
						collapsible
						defaultValue={resolved.variants.length <= 6 ? ['variants'] : []}
					>
						{#if resolved.description}
							<Accordion.Item value="description">
								<h3>
									<Accordion.ItemTrigger
										class="flex items-center justify-between gap-2 font-semibold"
									>
										Description
										<Accordion.ItemIndicator class="group">
											<ChevronDownIcon
												class="size-5 transition group-data-[state=open]:rotate-180"
											/>
										</Accordion.ItemIndicator>
									</Accordion.ItemTrigger>
								</h3>
								<Accordion.ItemContent>
									<p class="text-sm whitespace-pre-wrap">{resolved.description}</p>
								</Accordion.ItemContent>
							</Accordion.Item>
						{/if}
						{#if resolved.description && resolved.variants.length > 0}
							<hr class="hr" />
						{/if}
						{#if resolved.variants.length > 0}
							<Accordion.Item value="variants">
								<h3>
									<Accordion.ItemTrigger
										class="flex items-center justify-between gap-2 font-semibold"
									>
										<span>
											{number(resolved.variants.length)}
											{resolved.variants.length === 1 ? 'variant' : 'variants'} offered
										</span>
										<Accordion.ItemIndicator class="group">
											<ChevronDownIcon
												class="size-5 transition group-data-[state=open]:rotate-180"
											/>
										</Accordion.ItemIndicator>
									</Accordion.ItemTrigger>
								</h3>
								<Accordion.ItemContent>
									<DataTable
										rows={resolved.variants}
										columns={variantColumns}
										rowKey={(v) => v.url}
										flush
									/>
								</Accordion.ItemContent>
							</Accordion.Item>
						{/if}
					</Accordion>
				{/if}
			</div>
		</Card>
	{/if}

	{#if job.artifacts.children.length > 0}
		<Card title="Playlist entries" count={number(job.artifacts.children.length)} flush>
			<DataTable
				rows={children}
				columns={childColumns}
				rowKey={(c) => c.id}
				rowHref={(c) => resolve('/(app)/jobs/[id]', { id: c.id })}
				rowLabel="View"
				flush
				class="p-2"
			/>
		</Card>
	{/if}

	<Card title="Log" count={job.log.length > 0 ? number(job.log.length) : undefined} flush>
		{#if job.log.length === 0}
			<p class="p-6 text-center text-sm text-surface-600-400">Nothing logged yet.</p>
		{:else}
			<CodeBlock
				wrap
				class="max-h-80 rounded-t-none"
				code={job.log
					.map(
						(entry: LogEntry) =>
							`${absolute(entry.at)}  ${(entry.stage ?? '').padEnd(9)} ${entry.message}`
					)
					.join('\n')}
			/>
		{/if}
	</Card>
{/if}

<Confirm
	bind:open={confirmDelete}
	title="Delete this job?"
	message="Its record and cached files go away."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>

<Confirm
	bind:open={confirmCancel}
	title="Cancel this capture?"
	confirmLabel="Cancel the capture"
	cancelLabel="Keep recording"
	danger
	onconfirm={cancel}
>
	<p class="text-sm text-surface-600-400">
		The <Duration value={recordedSecs} /> recorded so far is deleted. Stop keeps it.
	</p>
</Confirm>
