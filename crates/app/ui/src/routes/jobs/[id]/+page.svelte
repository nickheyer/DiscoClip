<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { STAGES, codecName, durationSeconds, jobs as api, messageOf } from '$lib/api';
	import type { Job, JobSummary, LogEntry, Stage, Variant } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import JobStatusBadge, { STAGE_LABELS } from '$lib/components/JobStatusBadge.svelte';
	import JsonView from '$lib/components/JsonView.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import ProgressBar from '$lib/components/ProgressBar.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatBytes, formatClock, formatDateTime, formatDuration, formatNumber, shortId } from '$lib/format';
	import { clock } from '$lib/state/clock.svelte';
	import { confirm } from '$lib/state/confirm.svelte';
	import { jobs as feed } from '$lib/state/jobs.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const canManage = $derived(session.can('manage_jobs'));
	const refresh = () => invalidate(`app:job:${data.job.id}`);

	// The job as loaded, with the feed's word on it since.

	type Overlay = Partial<Pick<Job, 'status' | 'started_at' | 'finished_at' | 'updated_at'>>;
	let overlay = $state<Overlay>({});
	let liveLog = $state<LogEntry[]>([]);
	$effect.pre(() => {
		void data.job.id;
		overlay = {};
		liveLog = [];
	});
	const job = $derived<Job>({ ...data.job, ...overlay });

	$effect(() => {
		const id = data.job.id;
		return feed.subscribe((event) => {
			if (event.job !== id) return;
			switch (event.kind) {
				case 'status':
					overlay = {
						status: event.status,
						finished_at: event.job_summary?.finished_at ?? overlay.finished_at,
						started_at: event.job_summary?.started_at ?? overlay.started_at,
						updated_at: event.job_summary?.updated_at ?? overlay.updated_at
					};
					if (event.status.status !== 'running') void refresh();
					break;
				case 'log':
					liveLog = [...liveLog, event.entry];
					break;
				case 'children':
					void refresh();
					break;
				case 'deleted':
					toast.info('This job was deleted.');
					void goto('/jobs');
					break;
				default:
					break;
			}
		});
	});

	const progress = $derived(feed.progress[job.id]);
	const resolved = $derived(job.artifacts.resolved);
	const title = $derived(resolved?.title ?? job.request.url);
	const running = $derived(job.status.status === 'queued' || job.status.status === 'running');
	const finished = $derived(!running);
	const log = $derived.by(() => {
		const seen = new Set(job.log.map((entry) => `${entry.at}|${entry.message}`));
		return [...job.log, ...liveLog.filter((entry) => !seen.has(`${entry.at}|${entry.message}`))];
	});

	interface StageRow {
		stage: Stage;
		label: string;
		state: 'done' | 'running' | 'failed' | 'pending' | 'skipped';
		started: string | null;
		ended: string | null;
		seconds: number | null;
	}

	const stageRows = $derived.by((): StageRow[] => {
		const timings = job.artifacts.timings;
		const failedAt = job.status.status === 'failed' ? job.status.stage : null;
		const runningAt = job.status.status === 'running' ? job.status.stage : null;
		return STAGES.map((stage) => {
			const timing = [...timings].reverse().find((t) => t.stage === stage) ?? null;
			let state: StageRow['state'] = 'pending';
			if (timing) {
				if (failedAt === stage) state = 'failed';
				else if (runningAt === stage) state = 'running';
				else state = 'done';
			} else if (finished) {
				state = 'skipped';
			}
			const started = timing?.started_at ?? null;
			const ended = timing?.ended_at ?? null;
			const end = ended ? new Date(ended).getTime() : state === 'running' ? clock.now : null;
			const seconds = started && end ? Math.max(0, (end - new Date(started).getTime()) / 1000) : null;
			return { stage, label: STAGE_LABELS[stage].replace(/ing$/, ''), state, started, ended, seconds };
		});
	});

	function progressUnit(stage: string): 'bytes' | 'segments' | 'time' | 'percent' {
		switch (stage) {
			case 'download':
				return 'bytes';
			case 'transcode':
				return 'time';
			default:
				return 'percent';
		}
	}

	function stageIcon(state: StageRow['state']): string {
		switch (state) {
			case 'done':
				return 'check';
			case 'failed':
				return 'x';
			case 'running':
				return 'activity';
			case 'skipped':
				return 'more';
			default:
				return 'clock';
		}
	}

	function sourceLabel(): string {
		const source = job.request.origin.source;
		if (source === 'discord') return 'Discord';
		if (source === 'local') return 'Web app';
		return source;
	}

	function describeVariant(v: Variant): string {
		const parts: string[] = [];
		if (v.width && v.height) parts.push(`${v.width}×${v.height}`);
		else if (v.height) parts.push(`${v.height}p`);
		if (v.fps) parts.push(`${Math.round(v.fps)} fps`);
		const video = codecName(v.video);
		const audio = codecName(v.audio);
		if (video) parts.push(video);
		if (audio) parts.push(audio);
		if (v.bitrate) parts.push(`${Math.round(v.bitrate / 1000)} kb/s`);
		if (v.size) parts.push(formatBytes(v.size));
		return parts.join(' · ');
	}

	// Actions

	let acting = $state<'retry' | 'cancel' | 'delete' | null>(null);

	async function retry() {
		acting = 'retry';
		try {
			const { id } = await api.retry(job.id);
			toast.ok('Queued again.');
			await goto(`/jobs/${id}`);
		} catch (cause) {
			toast.error(`Could not retry: ${messageOf(cause)}`);
		} finally {
			acting = null;
		}
	}

	async function cancel() {
		acting = 'cancel';
		try {
			await api.cancel(job.id);
			toast.ok('Cancelled.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not cancel: ${messageOf(cause)}`);
		} finally {
			acting = null;
		}
	}

	async function remove() {
		const ok = await confirm.ask({
			title: 'Delete this job?',
			message: 'Its record and cached files go away. Anything already posted or archived stays where it is.',
			confirmLabel: 'Delete',
			danger: true
		});
		if (!ok) return;
		acting = 'delete';
		try {
			await api.remove(job.id);
			feed.forget(job.id);
			toast.ok('Deleted.');
			await goto('/jobs');
		} catch (cause) {
			toast.error(`Could not delete: ${messageOf(cause)}`);
			acting = null;
		}
	}

	function childTitle(child: JobSummary): string {
		return child.title ?? child.url;
	}

	const files = $derived([
		{ kind: 'output' as const, label: 'Output', file: job.artifacts.output },
		{ kind: 'source' as const, label: 'Source', file: job.artifacts.source }
	]);

	let showRequest = $state(false);
	let showResolved = $state(false);
</script>

<svelte:head>
	<title>{title} · Jobs · DiscoClip</title>
</svelte:head>

<PageHeader {title} crumbs={[{ label: 'Jobs', href: '/jobs' }, { label: shortId(job.id) }]}>
	{#snippet meta()}
		<JobStatusBadge status={job.status} />
		<span class="row faint small"><code>{job.id}</code><CopyButton text={job.id} square label="Copy job id" /></span>
		{#if job.request.parent}
			<a class="small" href={`/jobs/${job.request.parent}`}>Entry of playlist {shortId(job.request.parent)}</a>
		{/if}
		{#if job.request.retry_of}
			<a class="small" href={`/jobs/${job.request.retry_of}`}>Retry of {shortId(job.request.retry_of)}</a>
		{/if}
	{/snippet}
	{#snippet actions()}
		{#if job.status.status === 'done' && job.artifacts.output}
			<Button variant="primary" icon="download" href={api.downloadUrl(job.id)} external>Download</Button>
		{/if}
		{#if canManage}
			{#if running}
				<Button icon="stop" loading={acting === 'cancel'} disabled={acting !== null} onclick={cancel}>Cancel</Button>
			{:else}
				<Button icon="restart" loading={acting === 'retry'} disabled={acting !== null} onclick={retry}>Retry</Button>
				<Button variant="danger-soft" icon="trash" loading={acting === 'delete'} disabled={acting !== null} onclick={remove}>Delete</Button>
			{/if}
		{/if}
	{/snippet}
</PageHeader>

<div class="stack-lg">
	{#if job.status.status === 'failed'}
		<Alert tone="danger" title={`Failed while ${STAGE_LABELS[job.status.stage].toLowerCase()}`} message={job.status.message} />
	{:else if job.status.status === 'cancelled'}
		<Alert tone="warn" message="This job was cancelled before it finished." />
	{/if}

	<section class="card">
		<div class="card-header">
			<h2>Progress</h2>
			{#if job.started_at && job.finished_at}
				<span class="faint small">Took {formatDuration((new Date(job.finished_at).getTime() - new Date(job.started_at).getTime()) / 1000)}</span>
			{:else if job.status.status === 'queued'}
				<span class="faint small">Waiting for a worker</span>
			{/if}
		</div>
		<div class="card-body">
			<ol class="stages">
				{#each stageRows as row (row.stage)}
					<li class={['stage', row.state]}>
						<span class="stage-icon"><Icon name={stageIcon(row.state)} size={14} /></span>
						<div class="stage-text">
							<span class="strong">{row.label}</span>
							<span class="faint small">
								{#if row.state === 'pending'}Not yet{:else if row.state === 'skipped'}Skipped{:else if row.seconds != null}{formatDuration(row.seconds)}{/if}
								{#if row.started}· <Time value={row.started} />{/if}
							</span>
							{#if row.state === 'running' && progress && progress.stage === row.stage}
								<ProgressBar progress={progress.progress} unit={progressUnit(row.stage)} size="sm" label={`${row.stage} progress`} />
							{/if}
						</div>
					</li>
				{/each}
			</ol>
		</div>
	</section>

	<div class="grid-2 top">
		<section class="card">
			<div class="card-header"><h2>Request</h2><Button size="sm" variant="ghost" onclick={() => (showRequest = !showRequest)}>{showRequest ? 'Hide JSON' : 'JSON'}</Button></div>
			<div class="card-body stack">
				<dl class="kv">
					<dt>Link</dt>
					<dd class="row"><a href={job.request.url} target="_blank" rel="noreferrer" class="break">{job.request.url}</a><CopyButton text={job.request.url} square label="Copy link" /></dd>
					<dt>From</dt>
					<dd>
						{sourceLabel()}
						{#if job.request.origin.url}· <a href={job.request.origin.url} target="_blank" rel="noreferrer">the message</a>{/if}
						<span class="faint small block mono">{job.request.origin.reference}</span>
					</dd>
					<dt>Submitted by</dt>
					<dd>{job.request.submitted_by ?? '—'}</dd>
					{#if job.request.destination}
						<dt>Posts to</dt>
						<dd><code>{job.request.destination}</code></dd>
					{/if}
					<dt>Limits</dt>
					<dd>
						{#if job.request.limits.max_height == null && job.request.limits.max_duration_secs == null && job.request.limits.max_source_bytes == null}
							<span class="faint">Server limits</span>
						{:else}
							{[
								job.request.limits.max_height != null ? `≤ ${job.request.limits.max_height}p` : null,
								job.request.limits.max_duration_secs != null ? `≤ ${formatDuration(job.request.limits.max_duration_secs)}` : null,
								job.request.limits.max_source_bytes != null ? `≤ ${formatBytes(job.request.limits.max_source_bytes)}` : null
							].filter(Boolean).join(' · ')}
						{/if}
					</dd>
					<dt>Options</dt>
					<dd>
						Subtitles: {job.request.options.subtitles}{#if job.request.options.subtitle_language} ({job.request.options.subtitle_language}){/if}
						{#if job.request.options.clip}
							<span class="block">Clip: {formatClock(durationSeconds(job.request.options.clip.start) ?? 0)} → {job.request.options.clip.end ? formatClock(durationSeconds(job.request.options.clip.end) ?? 0) : 'end'}</span>
						{/if}
					</dd>
					<dt>Created</dt>
					<dd><Time value={job.created_at} mode="absolute" /></dd>
					{#if job.started_at}<dt>Started</dt><dd><Time value={job.started_at} mode="absolute" /></dd>{/if}
					{#if job.finished_at}<dt>Finished</dt><dd><Time value={job.finished_at} mode="absolute" /></dd>{/if}
				</dl>
				{#if showRequest}<JsonView value={job.request} />{/if}
			</div>
		</section>

		<section class="card">
			<div class="card-header"><h2>Media</h2>{#if resolved}<Button size="sm" variant="ghost" onclick={() => (showResolved = !showResolved)}>{showResolved ? 'Hide JSON' : 'JSON'}</Button>{/if}</div>
			<div class="card-body stack">
				{#if !resolved}
					<p class="muted">Nothing resolved yet.</p>
				{:else}
					<div class="media">
						{#if resolved.thumbnail}
							<img class="thumb" src={resolved.thumbnail} alt="" loading="lazy" />
						{/if}
						<dl class="kv">
							<dt>Title</dt>
							<dd>{resolved.title ?? '—'}</dd>
							<dt>Resolver</dt>
							<dd>{resolved.resolver}{#if resolved.id} · <code>{resolved.id}</code>{/if}</dd>
							{#if resolved.uploader}
								<dt>Uploader</dt>
								<dd>{#if resolved.uploader_url}<a href={resolved.uploader_url} target="_blank" rel="noreferrer">{resolved.uploader}</a>{:else}{resolved.uploader}{/if}</dd>
							{/if}
							{#if resolved.uploaded_at}<dt>Uploaded</dt><dd><Time value={resolved.uploaded_at} mode="absolute" /></dd>{/if}
							<dt>Duration</dt>
							<dd>{durationSeconds(resolved.duration) != null ? formatClock(durationSeconds(resolved.duration)!) : '—'}{#if resolved.live} <Badge tone="danger" size="sm">live</Badge>{/if}{#if resolved.age_limit} <Badge tone="warn" size="sm">{resolved.age_limit}+</Badge>{/if}</dd>
							{#if resolved.webpage_url}<dt>Page</dt><dd><a href={resolved.webpage_url} target="_blank" rel="noreferrer" class="break">{resolved.webpage_url}</a></dd>{/if}
							{#if resolved.clip}<dt>Clip</dt><dd>{formatClock(durationSeconds(resolved.clip.start) ?? 0)} → {resolved.clip.end ? formatClock(durationSeconds(resolved.clip.end) ?? 0) : 'end'}</dd>{/if}
						</dl>
					</div>
					{#if resolved.description}<p class="muted small description">{resolved.description}</p>{/if}
					{#if resolved.variants.length}
						<details>
							<summary class="small strong">{formatNumber(resolved.variants.length)} variant{resolved.variants.length === 1 ? '' : 's'} offered</summary>
							<div class="table-wrap inner">
								<table class="table">
									<thead><tr><th>Kind</th><th>Picture</th><th>Label</th><th>DRM</th></tr></thead>
									<tbody>
										{#each resolved.variants as v, i (i)}
											<tr>
												<td><code>{v.kind}</code>{#if v.video_only} <span class="faint small">video only</span>{/if}{#if v.audio_only} <span class="faint small">audio only</span>{/if}</td>
												<td>{describeVariant(v) || '—'}</td>
												<td>{v.label ?? v.format_id ?? '—'}</td>
												<td>{v.drm ?? '—'}</td>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
						</details>
					{/if}
					{#if resolved.subtitles.length}
						<p class="faint small">{formatNumber(resolved.subtitles.length)} subtitle track{resolved.subtitles.length === 1 ? '' : 's'} offered: {resolved.subtitles.map((t) => t.language + (t.auto ? ' (auto)' : '')).join(', ')}</p>
					{/if}
					{#if showResolved}<JsonView value={resolved} />{/if}
				{/if}
			</div>
		</section>
	</div>

	<section class="card">
		<div class="card-header"><h2>Artifacts</h2></div>
		{#if !job.artifacts.source && !job.artifacts.output && !job.artifacts.published && !job.artifacts.archived && job.artifacts.subtitles.length === 0}
			<div class="card-body"><p class="muted">Nothing has been produced yet.</p></div>
		{:else}
			<div class="card-body stack">
				{#if job.status.status === 'done' && job.artifacts.output}
					<video class="preview" controls preload="metadata" src={api.downloadUrl(job.id, 'output', 0, true)}>
						<track kind="captions" />
					</video>
				{/if}
				<div class="grid-2">
					{#each files as { kind, label, file } (kind)}
						{#if file}
							<div class="artifact">
								<div class="row-between">
									<span class="strong">{label}</span>
									<Button size="sm" icon="download" href={api.downloadUrl(job.id, kind)} external>Download</Button>
								</div>
								<dl class="kv small">
									<dt>Size</dt><dd>{formatBytes(file.size)}</dd>
									{#if file.info}
										<dt>Container</dt><dd>{codecName(file.info.container)}</dd>
										{#if file.info.video}<dt>Video</dt><dd>{codecName(file.info.video.codec)} {file.info.video.width}×{file.info.video.height}{#if file.info.video.fps} @ {file.info.video.fps.toFixed(2)} fps{/if}{#if file.info.video.bitrate} · {Math.round(file.info.video.bitrate / 1000)} kb/s{/if}</dd>{/if}
										{#if file.info.audio}<dt>Audio</dt><dd>{codecName(file.info.audio.codec)} {file.info.audio.channels}ch {file.info.audio.sample_rate} Hz{#if file.info.audio.bitrate} · {Math.round(file.info.audio.bitrate / 1000)} kb/s{/if}</dd>{/if}
										{#if durationSeconds(file.info.duration) != null}<dt>Duration</dt><dd>{formatClock(durationSeconds(file.info.duration)!)}</dd>{/if}
									{/if}
									<dt>Path</dt><dd class="mono break">{file.path}</dd>
								</dl>
							</div>
						{/if}
					{/each}
				</div>
				{#if job.artifacts.subtitles.length}
					<div class="artifact">
						<span class="strong">Subtitles</span>
						<ul class="plain">
							{#each job.artifacts.subtitles as track, i (track.path)}
								<li class="row"><code>{track.language}</code>{#if track.name}<span>{track.name}</span>{/if}<span class="faint small">{track.format}</span><Button size="sm" variant="ghost" icon="download" href={api.downloadUrl(job.id, 'subtitle', i)} external>Download</Button></li>
							{/each}
						</ul>
					</div>
				{/if}
				{#if job.artifacts.published}
					<div class="artifact">
						<span class="strong">Published</span>
						<dl class="kv small">
							<dt>Where</dt><dd>{#if job.artifacts.published.url}<a href={job.artifacts.published.url} target="_blank" rel="noreferrer" class="break">{job.artifacts.published.url}</a>{:else}<span class="mono break">{job.artifacts.published.reference}</span>{/if}</dd>
							<dt>When</dt><dd><Time value={job.artifacts.published.at} mode="absolute" /></dd>
						</dl>
					</div>
				{/if}
				{#if job.artifacts.archived}
					<div class="artifact">
						<span class="strong">Archived</span>
						<dl class="kv small">
							<dt>Files</dt><dd>{#each job.artifacts.archived.files as file (file)}<span class="mono break block">{file}</span>{/each}</dd>
							<dt>Size</dt><dd>{formatBytes(job.artifacts.archived.bytes)}</dd>
							<dt>When</dt><dd><Time value={job.artifacts.archived.at} mode="absolute" /></dd>
						</dl>
					</div>
				{/if}
			</div>
		{/if}
	</section>

	{#if job.artifacts.children.length}
		<section class="card">
			<div class="card-header"><h2>Playlist entries</h2><Button size="sm" variant="ghost" href={`/jobs?parent=${job.id}`} iconRight="chevron-right">Filter the list</Button></div>
			{#if data.children.length === 0}
				<div class="card-body"><Empty compact icon="video" title="No entries left" description="Every entry of this playlist was deleted." /></div>
			{:else}
				<div class="table-wrap flush">
					<table class="table">
						<thead><tr><th>Status</th><th>Entry</th><th>Media</th><th>When</th></tr></thead>
						<tbody>
							{#each data.children as child (child.id)}
								{@const live = feed.summary(child.id) ?? child}
								<tr>
									<td><JobStatusBadge status={live.status} size="sm" short /></td>
									<td><a href={`/jobs/${child.id}`} class="row-link">{childTitle(live)}</a></td>
									<td class="nowrap">{live.duration_secs != null ? formatClock(live.duration_secs) : '—'}</td>
									<td class="nowrap"><Time value={live.finished_at ?? live.created_at} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
	{/if}

	<section class="card">
		<div class="card-header"><h2>Log</h2><span class="faint small">{formatNumber(log.length)} {log.length === 1 ? 'line' : 'lines'}{#if running} · live{/if}</span></div>
		{#if log.length === 0}
			<div class="card-body"><p class="muted">Nothing logged yet.</p></div>
		{:else}
			<ol class="log">
				{#each log as entry, i (`${entry.at}-${i}`)}
					<li class="line">
						<span class="faint small mono when" title={formatDateTime(entry.at, true)}>{formatDateTime(entry.at, true)}</span>
						<span class="faint small stage-tag">{entry.stage ?? '—'}</span>
						<span class="message">{entry.message}</span>
					</li>
				{/each}
			</ol>
		{/if}
	</section>
</div>

<style>
	.top {
		align-items: start;
	}

	.stages {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 12px;
	}

	.stage {
		display: flex;
		gap: 10px;
		align-items: flex-start;
		padding: 10px 12px;
		border-radius: var(--radius-sm);
		background: var(--surface-2);
		border: 1px solid var(--border);
	}

	.stage-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		border-radius: 50%;
		background: var(--surface-3);
		color: var(--text-3);
		flex: none;
	}

	.stage.done .stage-icon {
		background: var(--ok-soft);
		color: var(--ok-text);
	}

	.stage.running {
		border-color: color-mix(in srgb, var(--accent) 50%, var(--border));
	}

	.stage.running .stage-icon {
		background: var(--accent-soft);
		color: var(--accent-text);
	}

	.stage.failed .stage-icon {
		background: var(--danger-soft);
		color: var(--danger-text);
	}

	.stage.skipped {
		opacity: 0.6;
	}

	.stage-text {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		flex: 1;
	}

	.media {
		display: flex;
		gap: 16px;
		align-items: flex-start;
	}

	.thumb {
		width: 160px;
		max-width: 40%;
		border-radius: var(--radius-sm);
		object-fit: cover;
		background: var(--surface-3);
		flex: none;
	}

	.media .kv {
		flex: 1;
		min-width: 0;
	}

	.description {
		white-space: pre-wrap;
		max-height: 8lh;
		overflow: auto;
	}

	.inner {
		margin-top: 8px;
		box-shadow: none;
	}

	.preview {
		width: 100%;
		max-height: 420px;
		border-radius: var(--radius-sm);
		background: #000;
	}

	.artifact {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 12px 14px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface-2);
	}

	.plain {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}

	.block {
		display: block;
	}

	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.log {
		list-style: none;
		margin: 0;
		padding: 8px 0;
		max-height: 480px;
		overflow: auto;
		font-size: 13px;
	}

	.line {
		display: grid;
		grid-template-columns: max-content 84px minmax(0, 1fr);
		gap: 12px;
		padding: 4px 20px;
		align-items: baseline;
	}

	.line:hover {
		background: var(--surface-2);
	}

	.stage-tag {
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.message {
		overflow-wrap: anywhere;
	}

	@media (max-width: 640px) {
		.media {
			flex-direction: column;
		}

		.line {
			grid-template-columns: 1fr;
			gap: 2px;
		}
	}
</style>
