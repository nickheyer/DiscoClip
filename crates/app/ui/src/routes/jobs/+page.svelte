<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import { SvelteSet } from 'svelte/reactivity';
	import type { PageData } from './$types';
	import { STATUS_KINDS, jobs as api, messageOf } from '$lib/api';
	import type { BulkAction, JobSummary, StatusKind, SubmitRequest, SubtitleMode } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import JobStatusBadge, { STATUS_LABELS } from '$lib/components/JobStatusBadge.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import ProgressBar from '$lib/components/ProgressBar.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatBytes, formatClock, formatNumber, parseTimeStamp, pluralize } from '$lib/format';
	import { confirm } from '$lib/state/confirm.svelte';
	import { jobs as feed } from '$lib/state/jobs.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { LIMITS } from './+page';

	let { data }: { data: PageData } = $props();

	const canManage = $derived(session.can('manage_jobs'));

	// Filters, bound to the address so a view can be shared and returned to.

	let q = $state('');
	let status = $state<StatusKind | ''>('');
	let source = $state('');
	let resolver = $state('');
	let topLevel = $state(false);
	let order = $state<'newest' | 'oldest'>('newest');
	let limit = $state(50);

	$effect.pre(() => {
		q = data.query.q ?? '';
		status = data.query.status ?? '';
		source = data.query.source ?? '';
		resolver = data.query.resolver ?? '';
		topLevel = data.query.top_level ?? false;
		order = data.query.order ?? 'newest';
		limit = data.query.limit ?? 50;
	});

	const filtered = $derived(
		Boolean(data.query.q || data.query.status || data.query.source || data.query.resolver || data.query.top_level || data.query.parent)
	);
	const resolvers = $derived(
		(data.stats.ok ? data.stats.value.resolvers : []).map((r) => r.resolver).sort()
	);

	function address(offset: number): string {
		const params = new URLSearchParams();
		if (q.trim()) params.set('q', q.trim());
		if (status) params.set('status', status);
		if (source) params.set('source', source);
		if (resolver) params.set('resolver', resolver);
		if (data.query.parent) params.set('parent', data.query.parent);
		if (topLevel) params.set('top_level', 'true');
		if (order === 'oldest') params.set('order', 'oldest');
		if (limit !== 50) params.set('limit', String(limit));
		if (offset > 0) params.set('offset', String(offset));
		const text = params.toString();
		return text ? `/jobs?${text}` : '/jobs';
	}

	function apply(event?: SubmitEvent) {
		event?.preventDefault();
		void goto(address(0), { keepFocus: true });
	}

	function clear() {
		void goto('/jobs');
	}

	// The rows: the page's list, kept current by the feed.

	let rows = $state<JobSummary[]>([]);
	let fresh = $state(0);
	$effect.pre(() => {
		rows = data.page.jobs;
		fresh = 0;
		feed.seed(data.page.jobs);
	});

	$effect(() => {
		return feed.subscribe((event) => {
			if (event.job_summary) {
				const index = rows.findIndex((row) => row.id === event.job);
				if (index >= 0) {
					rows[index] = event.job_summary;
				} else if (event.kind === 'submitted') {
					fresh += 1;
				}
			}
			if (event.kind === 'deleted') {
				rows = rows.filter((row) => row.id !== event.job);
				selected.delete(event.job);
			}
		});
	});

	const offset = $derived(data.page.offset);
	const total = $derived(data.page.total);
	const from = $derived(total === 0 ? 0 : offset + 1);
	const to = $derived(Math.min(total, offset + rows.length));

	// Selection and bulk actions

	const selected = new SvelteSet<string>();
	const allSelected = $derived(rows.length > 0 && rows.every((row) => selected.has(row.id)));

	function toggleAll(on: boolean) {
		if (on) for (const row of rows) selected.add(row.id);
		else selected.clear();
	}

	let acting = $state<string | null>(null);

	async function runBulk(action: BulkAction) {
		const ids = [...selected];
		if (ids.length === 0) return;
		if (action === 'delete') {
			const ok = await confirm.ask({
				title: `Delete ${pluralize(ids.length, 'job')}?`,
				message: 'Their records and cached files go away. Jobs still running are skipped; cancel them first.',
				confirmLabel: 'Delete',
				danger: true
			});
			if (!ok) return;
		}
		acting = action;
		try {
			const response = await api.bulk({ action, ids });
			const verb = action === 'retry' ? 'Retried' : action === 'cancel' ? 'Cancelled' : 'Deleted';
			if (response.failed === 0) {
				toast.ok(`${verb} ${pluralize(response.succeeded, 'job')}.`);
			} else {
				const first = response.results.find((r) => !r.ok);
				toast.error(`${verb} ${response.succeeded}, ${response.failed} failed: ${first?.error ?? 'unknown error'}`);
			}
			selected.clear();
			await invalidate('app:jobs');
		} catch (cause) {
			toast.error(`Could not ${action} the jobs: ${messageOf(cause)}`);
		} finally {
			acting = null;
		}
	}

	async function act(job: JobSummary, action: BulkAction) {
		if (action === 'delete') {
			const ok = await confirm.ask({
				title: 'Delete this job?',
				message: 'Its record and cached files go away.',
				confirmLabel: 'Delete',
				danger: true
			});
			if (!ok) return;
		}
		acting = `${action}:${job.id}`;
		try {
			if (action === 'retry') {
				const { id } = await api.retry(job.id);
				toast.ok('Queued again.');
				await goto(`/jobs/${id}`);
				return;
			}
			if (action === 'cancel') {
				await api.cancel(job.id);
				toast.ok('Cancelled.');
			} else {
				await api.remove(job.id);
				toast.ok('Deleted.');
			}
			await invalidate('app:jobs');
		} catch (cause) {
			toast.error(`Could not ${action}: ${messageOf(cause)}`);
		} finally {
			acting = null;
		}
	}

	// Submitting a link

	let dialog = $state(false);
	let url = $state('');
	let clipStart = $state('');
	let clipEnd = $state('');
	let subtitles = $state<SubtitleMode>('keep');
	let subtitleLanguage = $state('');
	let maxHeight = $state('');
	let maxMinutes = $state('');
	let maxMb = $state('');
	let submitting = $state(false);
	let submitError = $state<string | null>(null);

	const urlProblem = $derived.by(() => {
		if (url.trim() === '') return null;
		try {
			const parsed = new URL(url.trim());
			return parsed.protocol === 'http:' || parsed.protocol === 'https:' ? null : 'Only http and https links can be fetched.';
		} catch {
			return 'That is not a link.';
		}
	});
	const clipStartProblem = $derived(clipStart.trim() && parseTimeStamp(clipStart) === null ? 'A time such as 90, 1:30 or 1m30s.' : null);
	const clipEndProblem = $derived.by(() => {
		if (!clipEnd.trim()) return null;
		const end = parseTimeStamp(clipEnd);
		if (end === null) return 'A time such as 90, 1:30 or 1m30s.';
		const start = parseTimeStamp(clipStart) ?? 0;
		return end <= start ? 'The end comes before the start.' : null;
	});
	const numberProblem = (text: string, integer: boolean) =>
		text.trim() && !(Number(text) > 0 && (!integer || Number.isInteger(Number(text)))) ? 'A positive number.' : null;
	const heightProblem = $derived(numberProblem(maxHeight, true));
	const minutesProblem = $derived(numberProblem(maxMinutes, false));
	const mbProblem = $derived(numberProblem(maxMb, false));
	const ready = $derived(
		url.trim() !== '' && !urlProblem && !clipStartProblem && !clipEndProblem && !heightProblem && !minutesProblem && !mbProblem
	);

	function openSubmit() {
		url = '';
		clipStart = '';
		clipEnd = '';
		subtitles = 'keep';
		subtitleLanguage = '';
		maxHeight = '';
		maxMinutes = '';
		maxMb = '';
		submitError = null;
		dialog = true;
	}

	function duration(seconds: number) {
		const secs = Math.floor(seconds);
		return { secs, nanos: Math.round((seconds - secs) * 1e9) };
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		submitting = true;
		submitError = null;
		const start = parseTimeStamp(clipStart);
		const end = parseTimeStamp(clipEnd);
		const request: SubmitRequest = {
			url: url.trim(),
			limits: {
				max_height: maxHeight.trim() ? Number(maxHeight) : null,
				max_duration_secs: maxMinutes.trim() ? Math.round(Number(maxMinutes) * 60) : null,
				max_source_bytes: maxMb.trim() ? Math.round(Number(maxMb) * 1024 * 1024) : null
			},
			options: {
				clip: start !== null || end !== null ? { start: duration(start ?? 0), end: end === null ? null : duration(end) } : null,
				subtitles,
				subtitle_language: subtitleLanguage.trim() || null
			}
		};
		try {
			const { id } = await api.submit(request);
			dialog = false;
			toast.ok('Queued.');
			await goto(`/jobs/${id}`);
		} catch (cause) {
			submitError = messageOf(cause);
		} finally {
			submitting = false;
		}
	}

	function sourceLabel(job: JobSummary): string {
		if (job.source === 'discord') return 'Discord';
		if (job.source === 'local') return 'Web app';
		return job.source;
	}

	function submitter(job: JobSummary): string {
		if (!job.submitted_by) return '';
		return job.submitted_by.replace(/^discord:/, 'user ');
	}

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

	function isLive(job: JobSummary): boolean {
		return job.status.status === 'queued' || job.status.status === 'running';
	}
</script>

<svelte:head>
	<title>Jobs · DiscoClip</title>
</svelte:head>

<PageHeader title="Jobs" description="Every link the engine has taken on, filtered, with what came of each.">
	{#snippet actions()}
		{#if canManage}
			<Button variant="primary" icon="plus" onclick={openSubmit}>Submit a link</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack">
	<form class="card filters" onsubmit={apply}>
		<div class="filters-grid">
			<Field label="Search" for="f-q">
				<input id="f-q" class="input" type="search" bind:value={q} placeholder="Link, title, submitter or id" />
			</Field>
			<Field label="Status" for="f-status">
				<select id="f-status" class="select" bind:value={status}>
					<option value="">Any</option>
					{#each STATUS_KINDS as kind (kind)}
						<option value={kind}>{STATUS_LABELS[kind]}</option>
					{/each}
				</select>
			</Field>
			<Field label="Source" for="f-source">
				<select id="f-source" class="select" bind:value={source}>
					<option value="">Any</option>
					<option value="discord">Discord</option>
					<option value="local">Web app</option>
				</select>
			</Field>
			<Field label="Resolver" for="f-resolver">
				<select id="f-resolver" class="select" bind:value={resolver}>
					<option value="">Any</option>
					{#each resolvers as name (name)}
						<option value={name}>{name}</option>
					{/each}
				</select>
			</Field>
			<Field label="Order" for="f-order">
				<select id="f-order" class="select" bind:value={order}>
					<option value="newest">Newest first</option>
					<option value="oldest">Oldest first</option>
				</select>
			</Field>
			<Field label="Per page" for="f-limit">
				<select id="f-limit" class="select" bind:value={limit}>
					{#each LIMITS as option (option)}
						<option value={option}>{option}</option>
					{/each}
				</select>
			</Field>
		</div>
		<div class="filters-foot">
			<label class="checkbox">
				<input type="checkbox" bind:checked={topLevel} />
				<span><span class="strong">Hide playlist entries</span></span>
			</label>
			{#if data.query.parent}
				<Badge tone="info">Entries of playlist <code>{data.query.parent.split('-')[0]}</code></Badge>
			{/if}
			<span class="grow"></span>
			<Button variant="ghost" onclick={clear} disabled={!filtered && limit === 50 && order === 'newest'}>Clear</Button>
			<Button type="submit" variant="primary" icon="filter">Apply</Button>
		</div>
	</form>

	{#if fresh > 0 && offset === 0 && order === 'newest'}
		<Alert tone="info" message={`${pluralize(fresh, 'new job')} since this page loaded.`}>
			<div><Button size="sm" onclick={() => invalidate('app:jobs')}>Show them</Button></div>
		</Alert>
	{/if}

	{#if canManage && selected.size > 0}
		<div class="card bulk">
			<span class="strong">{pluralize(selected.size, 'job')} selected</span>
			<span class="grow"></span>
			<Button size="sm" icon="restart" loading={acting === 'retry'} disabled={acting !== null} onclick={() => runBulk('retry')}>Retry</Button>
			<Button size="sm" icon="stop" loading={acting === 'cancel'} disabled={acting !== null} onclick={() => runBulk('cancel')}>Cancel</Button>
			<Button size="sm" variant="danger-soft" icon="trash" loading={acting === 'delete'} disabled={acting !== null} onclick={() => runBulk('delete')}>Delete</Button>
			<Button size="sm" variant="ghost" onclick={() => selected.clear()}>Clear selection</Button>
		</div>
	{/if}

	{#if rows.length === 0}
		<Empty icon="video" title={filtered ? 'Nothing matches' : 'No jobs yet'} description={filtered ? 'No job matches these filters.' : 'Links posted in watched channels, sent with /clip, or submitted here show up as they run.'}>
			{#if filtered}<Button onclick={clear}>Clear filters</Button>{/if}
			{#if canManage && !filtered}<Button variant="primary" icon="plus" onclick={openSubmit}>Submit a link</Button>{/if}
		</Empty>
	{:else}
		<div class="table-wrap">
			<table class="table">
				<thead>
					<tr>
						{#if canManage}
							<th class="check"><input type="checkbox" checked={allSelected} onchange={(e) => toggleAll((e.currentTarget as HTMLInputElement).checked)} aria-label="Select every job on this page" /></th>
						{/if}
						<th>Status</th>
						<th>Job</th>
						<th>From</th>
						<th>Resolver</th>
						<th>Media</th>
						<th>When</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each rows as job (job.id)}
						{@const progress = feed.progress[job.id]}
						<tr class={[selected.has(job.id) && 'picked']}>
							{#if canManage}
								<td class="check"><input type="checkbox" checked={selected.has(job.id)} onchange={(e) => ((e.currentTarget as HTMLInputElement).checked ? selected.add(job.id) : selected.delete(job.id))} aria-label={`Select job ${job.id}`} /></td>
							{/if}
							<td class="status">
								<JobStatusBadge status={job.status} size="sm" short />
								{#if job.status.status === 'running' && progress}
									<ProgressBar progress={progress.progress} unit={progressUnit(progress.stage)} size="sm" label={`${progress.stage} progress`} />
								{/if}
							</td>
							<td class="job">
								<a href={`/jobs/${job.id}`} class="row-link truncate title" title={job.url}>{job.title ?? job.url}</a>
								{#if job.title}<span class="faint small truncate">{job.url}</span>{/if}
								{#if job.status.status === 'failed'}<span class="error-text small truncate" title={job.status.message}>{job.status.message}</span>{/if}
								{#if job.children > 0}<span class="faint small">Playlist · <a href={`/jobs?parent=${job.id}`}>{pluralize(job.children, 'entry', 'entries')}</a></span>{/if}
							</td>
							<td>
								<span>{sourceLabel(job)}</span>
								{#if submitter(job)}<span class="faint small block">{submitter(job)}</span>{/if}
							</td>
							<td>{job.resolver ?? '—'}</td>
							<td class="nowrap">
								{#if job.duration_secs != null}{formatClock(job.duration_secs)}{/if}
								{#if job.live}<Badge tone="danger" size="sm">live</Badge>{/if}
								{#if job.output_bytes != null}<span class="faint small block">{formatBytes(job.output_bytes)}</span>{/if}
							</td>
							<td class="nowrap"><Time value={job.finished_at ?? job.started_at ?? job.created_at} /></td>
							<td class="actions">
								{#if job.status.status === 'done'}
									<Button size="sm" variant="ghost" icon="download" href={api.downloadUrl(job.id)} external title="Download the output" square />
								{/if}
								{#if canManage}
									{#if isLive(job)}
										<Button size="sm" variant="ghost" icon="stop" loading={acting === `cancel:${job.id}`} onclick={() => act(job, 'cancel')} title="Cancel" square />
									{:else}
										<Button size="sm" variant="ghost" icon="restart" loading={acting === `retry:${job.id}`} onclick={() => act(job, 'retry')} title="Retry" square />
										<Button size="sm" variant="ghost" icon="trash" loading={acting === `delete:${job.id}`} onclick={() => act(job, 'delete')} title="Delete" square />
									{/if}
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		<div class="row-between">
			<span class="faint small">Showing {formatNumber(from)}–{formatNumber(to)} of {pluralize(total, 'job')}</span>
			<div class="row">
				<Button size="sm" icon="chevron-left" href={address(Math.max(0, offset - limit))} disabled={offset === 0}>Newer</Button>
				<Button size="sm" iconRight="chevron-right" href={address(offset + limit)} disabled={offset + limit >= total}>Older</Button>
			</div>
		</div>
	{/if}
</div>

<Dialog bind:open={dialog} title="Submit a link" description="The engine resolves it, downloads the video, transcodes it to fit, and writes it to the server's local directory." busy={submitting}>
	<form id="submit-form" class="stack" onsubmit={submit} novalidate>
		{#if submitError}
			<Alert tone="danger" message={submitError} onclose={() => (submitError = null)} />
		{/if}
		<Field label="Link" for="s-url" error={urlProblem} hint="A video page, a playlist, or a direct media or manifest URL.">
			<input id="s-url" class="input" type="url" bind:value={url} placeholder="https://" required aria-invalid={urlProblem ? 'true' : undefined} />
		</Field>
		<div class="grid-2">
			<Field label="Clip from" for="s-start" optional hint="90, 1:30 or 1m30s." error={clipStartProblem}>
				<input id="s-start" class="input mono" bind:value={clipStart} placeholder="Start" aria-invalid={clipStartProblem ? 'true' : undefined} />
			</Field>
			<Field label="Clip to" for="s-end" optional error={clipEndProblem}>
				<input id="s-end" class="input mono" bind:value={clipEnd} placeholder="End" aria-invalid={clipEndProblem ? 'true' : undefined} />
			</Field>
		</div>
		<div class="grid-2">
			<Field label="Subtitles" for="s-subs">
				<select id="s-subs" class="select" bind:value={subtitles}>
					<option value="keep">Keep beside the video</option>
					<option value="burn">Burn into the picture</option>
					<option value="skip">Ignore</option>
				</select>
			</Field>
			<Field label="Subtitle language" for="s-lang" optional hint="Such as en or de; every language when empty.">
				<input id="s-lang" class="input" bind:value={subtitleLanguage} placeholder="en" maxlength="16" />
			</Field>
		</div>
		<div class="grid-3">
			<Field label="Tallest output" for="s-height" optional error={heightProblem}>
				<input id="s-height" class="input" type="number" min="0" step="1" bind:value={maxHeight} placeholder="px" aria-invalid={heightProblem ? 'true' : undefined} />
			</Field>
			<Field label="Longest video" for="s-minutes" optional error={minutesProblem}>
				<input id="s-minutes" class="input" type="number" min="0" step="any" bind:value={maxMinutes} placeholder="min" aria-invalid={minutesProblem ? 'true' : undefined} />
			</Field>
			<Field label="Largest source" for="s-mb" optional error={mbProblem}>
				<input id="s-mb" class="input" type="number" min="0" step="any" bind:value={maxMb} placeholder="MB" aria-invalid={mbProblem ? 'true' : undefined} />
			</Field>
		</div>
		<p class="hint">Limits tighten the server's own; they never loosen them.</p>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={submitting}>Cancel</Button>
		<Button variant="primary" loading={submitting} disabled={!ready} onclick={() => document.querySelector<HTMLFormElement>('#submit-form')?.requestSubmit()}>Queue it</Button>
	{/snippet}
</Dialog>

<style>
	.filters {
		display: flex;
		flex-direction: column;
	}

	.filters-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
		gap: 14px;
		padding: 18px 20px;
	}

	.filters-foot {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
		padding: 12px 20px;
		border-top: 1px solid var(--border);
		background: var(--surface-2);
		border-radius: 0 0 var(--radius) var(--radius);
	}

	.grow {
		flex: 1;
	}

	.bulk {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
		padding: 10px 16px;
		border-color: color-mix(in srgb, var(--accent) 40%, var(--border));
	}

	th.check,
	td.check {
		width: 32px;
		padding-right: 0;
	}

	td.status {
		min-width: 150px;
	}

	td.status :global(.bar) {
		margin-top: 6px;
	}

	td.job {
		max-width: 420px;
	}

	.title {
		display: block;
		max-width: 100%;
	}

	.block {
		display: block;
	}

	tr.picked td {
		background: color-mix(in srgb, var(--accent-soft) 60%, transparent);
	}
</style>
