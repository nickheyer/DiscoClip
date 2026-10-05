<script lang="ts">
	import ArrowRightIcon from '@lucide/svelte/icons/arrow-right';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import { Progress } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { applications, health as healthApi, jobs } from '$lib/api/endpoints';
	import type {
		ApplicationView,
		Health,
		JobStats,
		JobSummary,
		Progress as JobProgress,
		Stage,
		Uuid
	} from '$lib/api/types';
	import Bytes from '$lib/components/Bytes.svelte';
	import Card from '$lib/components/Card.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import JobTitle from '$lib/components/JobTitle.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PlaceLine from '$lib/components/PlaceLine.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import StatTile from '$lib/components/StatTile.svelte';
	import Status from '$lib/components/Status.svelte';
	import { feed } from '$lib/events.svelte';
	import { number, percent } from '$lib/format';
	import { mergeJobEvent } from '$lib/live';
	import { session } from '$lib/session.svelte';
	import { submitDialog } from '$lib/submit.svelte';

	const HEALTH_EVERY = 30_000;
	const RECENT = 10;

	let stats = $state<JobStats | null>(null);
	let active = $state<JobSummary[]>([]);
	let progress = $state<Record<Uuid, { stage: Stage; progress: JobProgress }>>({});
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

	/** Every job in flight, then the latest finished ones, each job once. */
	const rows = $derived.by((): JobSummary[] => {
		const seen = new Set(active.map((job) => job.id));
		return [...active, ...recent.filter((job) => !seen.has(job.id))];
	});

	const columns: Column<JobSummary>[] = [
		{ key: 'title', label: 'Job', cell: titleCell, class: 'min-w-64', fill: true },
		{ key: 'status', label: 'Status', cell: statusCell, class: 'whitespace-nowrap' },
		{ key: 'source', label: 'From', cell: fromCell },
		{ key: 'size', label: 'Size', align: 'right', cell: sizeCell, class: 'whitespace-nowrap' },
		{ key: 'age', label: 'Submitted', cell: ageCell, class: 'whitespace-nowrap' }
	];
</script>

{#snippet titleCell(job: JobSummary)}
	<JobTitle {job} />
{/snippet}
{#snippet statusCell(job: JobSummary)}
	{@const p = job.status.status === 'running' ? progress[job.id] : undefined}
	<div class="space-y-1.5">
		<Status job={job.status} />
		{#if p}
			<!-- Skeleton's linear progress: a real bar while the total is known, indeterminate otherwise. -->
			<Progress
				value={p.progress.total ? p.progress.done : null}
				max={p.progress.total ?? 100}
				class="grid w-40 grid-cols-[minmax(0,1fr)_auto] items-center gap-2"
			>
				<Progress.Track class="h-1">
					<Progress.Range class="bg-primary-500" />
				</Progress.Track>
				<Progress.ValueText class="text-xs text-surface-600-400 tabular-nums">
					{#if p.progress.total}{percent(p.progress.done / p.progress.total)}{:else}{number(
							p.progress.done
						)}{/if}
				</Progress.ValueText>
			</Progress>
		{/if}
	</div>
{/snippet}
{#snippet fromCell(job: JobSummary)}
	<PlaceLine origin={job.origin} place={job.place} destination={job.destination} compact />
{/snippet}
{#snippet sizeCell(job: JobSummary)}
	<Bytes value={job.output_bytes} />
{/snippet}
{#snippet ageCell(job: JobSummary)}
	<Timestamp at={job.created_at} class="whitespace-nowrap" />
{/snippet}

<PageHeader title="Dashboard" description="Jobs, workers and connected applications at a glance.">
	{#snippet actions()}
		{#if session.can('manage_jobs')}
			<button
				type="button"
				class="btn preset-filled-primary-500"
				onclick={() => (submitDialog.open = true)}
			>
				<PlusIcon class="size-4" />
				Submit a link
			</button>
		{/if}
	{/snippet}
</PageHeader>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<section class="grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-5" aria-label="Job counts">
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
	</section>

	<div class="grid min-w-0 items-start gap-6 xl:grid-cols-[minmax(0,1fr)_20rem]">
		<Card
			title="Jobs"
			count={loading ? undefined : `${number(active.length)} active`}
			description="Running and queued jobs first, then the latest finished ones."
			flush
		>
			{#snippet actions()}
				<a href={resolve('/jobs')} class="btn preset-tonal btn-sm">
					All jobs <ArrowRightIcon class="size-4" />
				</a>
			{/snippet}
			<DataTable
				{rows}
				{columns}
				rowKey={(job) => job.id}
				{loading}
				rowHref={(job) => resolve('/(app)/jobs/[id]', { id: job.id })}
				rowLabel="View"
				flush
				class="p-2"
			>
				{#snippet empty()}
					<p class="font-medium">No jobs yet</p>
					<p class="mt-1">
						{session.can('manage_jobs')
							? 'Submit a media link to start one. Its progress shows here as it runs.'
							: 'Jobs appear here with their progress as they run.'}
					</p>
				{/snippet}
			</DataTable>
		</Card>

		<div class="grid content-start gap-6 sm:grid-cols-2 xl:grid-cols-1">
			{#if canBots}
				<Card title="Applications">
					<div class="space-y-3">
						{#if loading}
							<div class="h-10 placeholder animate-pulse" aria-label="Loading applications"></div>
						{:else if apps.length === 0}
							<p class="text-sm text-surface-600-400">
								Connect a Discord application to receive links from your servers.
							</p>
						{:else}
							<ul class="space-y-2">
								{#each apps as app (app.id)}
									{@const bot = feed.bots[app.id]}
									<li class="flex flex-wrap items-center justify-between gap-2 text-sm">
										<a
											href={resolve('/(app)/applications/[id]', { id: app.id })}
											class="min-w-0 truncate anchor font-medium">{app.name}</a
										>
										{#if bot}
											<Status bot={bot.state} />
										{:else}
											<span class="text-surface-600-400">Awaiting status</span>
										{/if}
									</li>
								{/each}
							</ul>
						{/if}
						<a
							href={resolve('/applications')}
							class="inline-flex items-center gap-1 anchor text-sm"
						>
							{apps.length === 0 ? 'Add an application' : 'Manage applications'}
							<ArrowRightIcon class="size-4" />
						</a>
					</div>
				</Card>
			{/if}

			<Card title="System health">
				{#snippet actions()}
					{#if health}<Status health={health.status} />{/if}
				{/snippet}
				<div class="space-y-3">
					{#if health}
						{#if failing.length === 0}
							<p class="text-sm text-surface-600-400">All checks are passing.</p>
						{:else}
							<ul class="space-y-2">
								{#each failing as check (check.name)}
									<li
										class="card p-3 text-sm {check.status === 'fail'
											? 'preset-tonal-error'
											: 'preset-tonal-warning'}"
									>
										<p class="font-medium">{check.label}</p>
										<p class="opacity-80">{check.detail}</p>
									</li>
								{/each}
							</ul>
						{/if}
						<a href={resolve('/health')} class="inline-flex items-center gap-1 anchor text-sm">
							All checks <ArrowRightIcon class="size-4" />
						</a>
					{:else}
						<div class="h-10 placeholder animate-pulse" aria-label="Loading health checks"></div>
					{/if}
				</div>
			</Card>
		</div>
	</div>
{/if}
