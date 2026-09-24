<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import ArrowRightIcon from '@lucide/svelte/icons/arrow-right';
	import BotIcon from '@lucide/svelte/icons/bot';
	import ListVideoIcon from '@lucide/svelte/icons/list-video';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { applications, health as healthApi, jobs } from '$lib/api/endpoints';
	import type {
		ApplicationView,
		Health,
		JobStats,
		JobSummary,
		Progress,
		Stage,
		Uuid
	} from '$lib/api/types';
	import Bytes from '$lib/components/Bytes.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import JobTitle from '$lib/components/JobTitle.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import StatTile from '$lib/components/StatTile.svelte';
	import Status from '$lib/components/Status.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import { feed } from '$lib/events.svelte';
	import { number, percent, stageLabel } from '$lib/format';
	import { mergeJobEvent } from '$lib/live';
	import { session } from '$lib/session.svelte';
	import { submitDialog } from '$lib/submit.svelte';

	const HEALTH_EVERY = 30_000;
	const RECENT = 10;

	let stats = $state<JobStats | null>(null);
	let active = $state<JobSummary[]>([]);
	let progress = $state<Record<Uuid, { stage: Stage; progress: Progress }>>({});
	let recent = $state<JobSummary[]>([]);
	let health = $state<Health | null>(null);
	let apps = $state<ApplicationView[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	const liveStats = $derived(feed.stats ?? stats);
	const canBots = $derived(session.can('manage_applications'));

	async function load() {
		loading = true;
		error = null;
		try {
			const [first, running, queued, latest, current] = await Promise.all([
				jobs.stats(),
				jobs.list({ status: 'running', limit: 100 }),
				jobs.list({ status: 'queued', limit: 100 }),
				jobs.list({ limit: RECENT, top_level: true }),
				healthApi.get()
			]);
			stats = first;
			active = [...running.jobs, ...queued.jobs];
			recent = latest.jobs;
			health = current;
			if (canBots) apps = await applications.list();
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	async function refreshHealth() {
		try {
			health = await healthApi.get();
		} catch (err) {
			error = err;
		}
	}

	onMount(() => {
		void load();
		const timer = setInterval(() => void refreshHealth(), HEALTH_EVERY);
		const stop = feed.onJob((event) => {
			active = mergeJobEvent(active, event, {
				insert: true,
				filter: (job) => job.status.status === 'running' || job.status.status === 'queued'
			});
			recent = mergeJobEvent(recent, event, {
				insert: true,
				max: RECENT,
				filter: (job) => job.parent === null
			});
			if (event.kind === 'progress') {
				progress = { ...progress, [event.job]: { stage: event.stage, progress: event.progress } };
			}
			if (
				event.kind === 'deleted' ||
				(event.kind === 'status' && event.status.status !== 'running')
			) {
				const rest = { ...progress };
				delete rest[event.job];
				progress = rest;
			}
		});
		return () => {
			clearInterval(timer);
			stop();
		};
	});

	const failing = $derived(health?.checks.filter((check) => check.status !== 'ok') ?? []);

	const columns: Column<JobSummary>[] = [
		{ key: 'title', label: 'Job', cell: titleCell, class: 'min-w-64' },
		{ key: 'status', label: 'Status', cell: statusCell },
		{ key: 'source', label: 'Source', value: (job) => job.source },
		{ key: 'size', label: 'Size', align: 'right', cell: sizeCell },
		{ key: 'age', label: 'Submitted', cell: ageCell }
	];
</script>

{#snippet titleCell(job: JobSummary)}
	<JobTitle {job} />
{/snippet}
{#snippet statusCell(job: JobSummary)}
	<Status job={job.status} />
{/snippet}
{#snippet sizeCell(job: JobSummary)}
	<Bytes value={job.output_bytes} />
{/snippet}
{#snippet ageCell(job: JobSummary)}
	<RelativeTime at={job.created_at} class="whitespace-nowrap" />
{/snippet}

<PageHeader title="Dashboard" />

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<Toolbar description="Monitor jobs, workers, and connected applications.">
		{#if session.can('manage_jobs')}
			<button
				type="button"
				class="btn preset-filled-primary-500"
				onclick={() => (submitDialog.open = true)}
			>
				<PlusIcon />
				Submit a link
			</button>
		{/if}
	</Toolbar>
	<section class="grid grid-cols-2 gap-4 md:grid-cols-3 2xl:grid-cols-6" aria-label="Job counts">
		<StatTile
			label="Queued"
			value={number(liveStats?.counts.queued ?? 0)}
			hint="{number(liveStats?.queue_depth ?? 0)} waiting for a worker"
			{loading}
		/>
		<StatTile
			label="Running"
			value={number(liveStats?.counts.running ?? 0)}
			hint="{number(liveStats?.utilisation.active ?? 0)} of {number(
				liveStats?.utilisation.workers ?? 0
			)} workers busy"
			tone="primary"
			{loading}
		/>
		<StatTile
			label="Done"
			value={number(liveStats?.counts.done ?? 0)}
			hint="{number(liveStats?.last_24h.done ?? 0)} in the last 24 hours"
			tone="success"
			{loading}
		/>
		<StatTile
			label="Failed"
			value={number(liveStats?.counts.failed ?? 0)}
			hint="{number(liveStats?.last_24h.failed ?? 0)} in the last 24 hours"
			tone={liveStats && liveStats.last_24h.failed > 0 ? 'error' : 'surface'}
			{loading}
		/>
		<StatTile
			label="Cancelled"
			value={number(liveStats?.counts.cancelled ?? 0)}
			hint="{number(liveStats?.last_24h.cancelled ?? 0)} in the last 24 hours"
			{loading}
		/>
		<StatTile
			label="Health"
			value={health?.status === 'ok'
				? 'Healthy'
				: health?.status === 'warn'
					? 'Warning'
					: health
						? 'Failing'
						: 'Unknown'}
			hint={health
				? failing.length === 0
					? 'All checks passing'
					: `${failing.length} check${failing.length === 1 ? ' needs' : 's need'} attention`
				: 'Waiting for checks'}
			tone={health?.status === 'ok'
				? 'success'
				: health?.status === 'warn'
					? 'warning'
					: health
						? 'error'
						: 'surface'}
			{loading}
		/>
	</section>

	<div class="grid min-w-0 gap-6 xl:grid-cols-[minmax(0,1fr)_22rem]">
		<section
			class="flex min-w-0 flex-col card border border-surface-200-800 bg-surface-100-900"
			aria-labelledby="active-jobs-heading"
		>
			<header
				class="flex flex-wrap items-center justify-between gap-3 border-b border-surface-200-800 px-5 py-4 sm:px-6"
			>
				<div class="flex items-center gap-3">
					<h2 id="active-jobs-heading" class="text-lg font-semibold">Active jobs</h2>
					{#if !loading}<span class="text-surface-600-400">{number(active.length)}</span>{/if}
				</div>
				<a href={resolve('/jobs')} class="btn btn-sm hover:preset-tonal"
					>View jobs <ArrowRightIcon /></a
				>
			</header>
			<div class="flex min-h-64 flex-1 flex-col p-5 sm:p-6">
				{#if loading}
					<div class="space-y-4" aria-busy="true" aria-label="Loading active jobs">
						{#each { length: 3 }, i (i)}<div class="h-16 placeholder animate-pulse"></div>{/each}
					</div>
				{:else if active.length === 0}
					<div class="flex flex-1 items-center justify-center">
						<EmptyState
							contained
							title="No active jobs"
							description={session.can('manage_jobs')
								? 'Submit a media link to start a job. Track its progress here as it runs.'
								: 'Queued and running jobs will appear here with their progress.'}
						>
							{#snippet icon()}<ListVideoIcon class="size-6" />{/snippet}
						</EmptyState>
					</div>
				{:else}
					<ul class="divide-y divide-surface-200-800">
						{#each active as job (job.id)}
							{@const p = progress[job.id]}
							<li class="space-y-3 py-4 first:pt-0 last:pb-0">
								<div class="flex flex-wrap items-center gap-3">
									<a
										href={resolve('/(app)/jobs/[id]', { id: job.id })}
										class="min-w-0 flex-1 rounded-base hover:text-primary-700-300"
										><JobTitle {job} /></a
									>
									<Status job={job.status} />
								</div>
								{#if p}
									<div class="flex flex-wrap items-center gap-3 text-sm text-surface-600-400">
										<progress
											class="progress h-2 min-w-24 flex-1"
											aria-label="Job progress"
											value={p.progress.total ? p.progress.done : undefined}
											max={p.progress.total ?? undefined}
										></progress>
										<span class="text-right tabular-nums">
											{#if p.progress.total}{percent(
													p.progress.done / p.progress.total
												)}{:else}{number(p.progress.done)}{/if} · {stageLabel(p.stage)}
										</span>
									</div>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</div>
		</section>

		<div class="grid content-start gap-6 sm:grid-cols-2 xl:grid-cols-1">
			{#if canBots}
				<section
					class="min-w-0 space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
					aria-label="Applications"
				>
					<div class="flex items-center justify-between gap-3">
						<h2 class="text-lg font-semibold">Applications</h2>
						<BotIcon class="size-5 text-surface-600-400" aria-hidden="true" />
					</div>
					{#if loading}
						<div class="h-12 placeholder animate-pulse" aria-label="Loading applications"></div>
					{:else if apps.length === 0}
						<p class="text-sm leading-relaxed text-surface-600-400">
							Connect a Discord application to receive links from your servers.
						</p>
					{:else}
						<ul class="space-y-3">
							{#each apps as app (app.id)}
								{@const bot = feed.bots[app.id]}
								<li class="flex flex-wrap items-center justify-between gap-2 text-sm">
									<a
										href={resolve('/(app)/applications/[id]', { id: app.id })}
										class="min-w-0 truncate font-medium hover:text-primary-700-300">{app.name}</a
									>
									{#if bot}<Status bot={bot.state} />{:else}<span class="text-surface-600-400"
											>Awaiting status</span
										>{/if}
								</li>
							{/each}
						</ul>
					{/if}
					<a
						href={resolve('/applications')}
						class="inline-flex min-h-9 items-center gap-2 anchor text-sm font-medium"
						>{apps.length === 0 ? 'Add an application' : 'Manage applications'}
						<ArrowRightIcon class="size-4" /></a
					>
				</section>
			{/if}

			<section
				class="min-w-0 space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Health"
			>
				<div class="flex flex-wrap items-center justify-between gap-3">
					<h2 class="text-lg font-semibold">System health</h2>
					{#if health}<Status health={health.status} />{/if}
				</div>
				{#if health}
					{#if failing.length === 0}
						<p class="text-sm text-surface-600-400">All checks are passing.</p>
					{:else}
						<ul class="space-y-4">
							{#each failing as check (check.name)}
								<li
									class="space-y-1 border-l-2 pl-3 text-sm {check.status === 'fail'
										? 'border-error-500'
										: 'border-warning-500'}"
								>
									<p class="font-medium">{check.label}</p>
									<p class="leading-relaxed text-surface-600-400">{check.detail}</p>
								</li>
							{/each}
						</ul>
					{/if}
					<a
						href={resolve('/health')}
						class="inline-flex min-h-9 items-center gap-2 anchor text-sm font-medium"
						>View all checks <ArrowRightIcon class="size-4" /></a
					>
				{:else}
					<div class="h-12 placeholder animate-pulse" aria-label="Loading health checks"></div>
				{/if}
			</section>
		</div>
	</div>

	<section class="min-w-0 space-y-4" aria-labelledby="recent-jobs-heading">
		<header class="flex flex-wrap items-center justify-between gap-3">
			<div class="space-y-1">
				<h2 id="recent-jobs-heading" class="text-lg font-semibold">Recent jobs</h2>
				<p class="text-sm text-surface-600-400">Your latest submissions and their results.</p>
			</div>
			<a href={resolve('/jobs')} class="btn preset-tonal btn-sm">All jobs <ArrowRightIcon /></a>
		</header>
		<DataTable
			rows={recent}
			{columns}
			rowKey={(job) => job.id}
			{loading}
			rowHref={(job) => resolve('/(app)/jobs/[id]', { id: job.id })}
			rowLabel="View"
		>
			{#snippet empty()}
				<p class="font-medium text-surface-950-50">No jobs yet</p>
				<p class="mt-2">Your submissions will appear here.</p>
			{/snippet}
		</DataTable>
	</section>
{/if}
