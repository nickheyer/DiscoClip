<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteMap } from 'svelte/reactivity';
	import { metrics as metricsApi } from '$lib/api/endpoints';
	import type { BotState, Metrics } from '$lib/api/types';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import StatTile from '$lib/components/StatTile.svelte';
	import Status from '$lib/components/Status.svelte';
	import { bytes, EMPTY, number, percent, span } from '$lib/format';

	const EVERY = 5_000;

	let metrics = $state<Metrics | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);

	async function load() {
		try {
			metrics = await metricsApi.get();
			error = null;
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void load();
		const timer = setInterval(() => void load(), EVERY);
		return () => clearInterval(timer);
	});

	const requests = $derived.by(() => {
		if (!metrics) return [];
		const byHost = new SvelteMap<
			string,
			{ host: string; total: number; ok: number; failed: number }
		>();
		for (const r of metrics.http.requests) {
			const row = byHost.get(r.host) ?? { host: r.host, total: 0, ok: 0, failed: 0 };
			row.total += r.count;
			if (r.status >= 400) row.failed += r.count;
			else row.ok += r.count;
			byHost.set(r.host, row);
		}
		return [...byHost.values()].sort((a, b) => b.total - a.total);
	});

	const BOT_STATES: BotState[] = [
		'connected',
		'starting',
		'retrying',
		'failed',
		'stopped',
		'disabled'
	];
</script>

<PageHeader title="Metrics" />

<Toolbar description="Resource use, job counts, requests and storage. Refreshes every 5 seconds.">
	{#if metrics}
		<span class="text-sm text-surface-600-400">Updated <RelativeTime at={metrics.at} /></span>
	{/if}
</Toolbar>

{#if error && !metrics && !loading}
	<ErrorState {error} onretry={load} />
{:else if !metrics}
	<div class="grid grid-cols-2 gap-3 md:grid-cols-4" aria-busy="true">
		{#each { length: 8 }, i (i)}<div class="h-24 placeholder animate-pulse"></div>{/each}
	</div>
{:else}
	{#if error}
		<p class="card preset-tonal-error p-3 text-sm" role="alert">
			The last refresh failed. Showing the result from <RelativeTime at={metrics.at} />.
		</p>
	{/if}

	<section class="space-y-3" aria-label="Process">
		<h2 class="h6">Process · version {metrics.version} · up {span(metrics.uptime_secs)}</h2>
		<div class="grid grid-cols-2 gap-3 md:grid-cols-4">
			{#if metrics.process}
				<StatTile
					label="CPU"
					value={percent(metrics.process.cpu_percent / 100, 1)}
					hint="of one core"
				/>
				<StatTile
					label="Memory"
					value={bytes(metrics.process.rss_bytes)}
					hint="resident · {bytes(metrics.process.virtual_bytes)} virtual"
				/>
				<StatTile
					label="PID"
					value={metrics.process.pid}
					hint="running {span(metrics.process.run_time_secs)}"
				/>
			{:else}
				<StatTile
					label="Process"
					value={EMPTY}
					hint="Process metrics are unavailable on this host"
				/>
			{/if}
			<StatTile
				label="Load"
				value={metrics.system.load_average.map((l) => l.toFixed(2)).join(' / ')}
				hint="1, 5 and 15 minutes · {number(metrics.system.cpus)} CPUs"
			/>
		</div>
	</section>

	<section class="space-y-3" aria-label="Host">
		<h2 class="h6">Host</h2>
		<div class="grid grid-cols-2 gap-3 md:grid-cols-4">
			<StatTile
				label="Memory free"
				value={bytes(metrics.system.available_memory_bytes)}
				hint="of {bytes(metrics.system.total_memory_bytes)}"
				tone={metrics.system.available_memory_bytes / metrics.system.total_memory_bytes < 0.1
					? 'warning'
					: 'surface'}
			/>
			{#each metrics.system.disks as disk (disk.mount)}
				{@const used = disk.total_bytes - disk.available_bytes}
				{@const ratio = disk.total_bytes > 0 ? used / disk.total_bytes : 0}
				<StatTile
					label={disk.mount}
					value={bytes(disk.available_bytes)}
					hint="free of {bytes(disk.total_bytes)} · holds {disk.holds.join(', ')}"
					tone={ratio > 0.9 ? 'error' : ratio > 0.8 ? 'warning' : 'surface'}
				>
					<meter
						class="meter mt-1 w-full"
						value={used}
						min="0"
						max={disk.total_bytes}
						low={disk.total_bytes * 0.8}
						high={disk.total_bytes * 0.9}
						optimum="0"
					></meter>
				</StatTile>
			{/each}
		</div>
	</section>

	<section class="space-y-3" aria-label="Jobs">
		<h2 class="h6">Jobs</h2>
		<div class="grid grid-cols-2 gap-3 md:grid-cols-4 lg:grid-cols-6">
			<StatTile
				label="Queued"
				value={number(metrics.jobs.counts.queued)}
				hint="{number(metrics.jobs.queue_depth)} waiting"
			/>
			<StatTile
				label="Running"
				value={number(metrics.jobs.counts.running)}
				hint="{number(metrics.jobs.utilisation.active)} of {number(
					metrics.jobs.utilisation.workers
				)} workers"
				tone="primary"
			/>
			<StatTile
				label="Done"
				value={number(metrics.jobs.counts.done)}
				hint="{number(metrics.jobs.last_24h.done)} in 24 h"
				tone="success"
			/>
			<StatTile
				label="Failed"
				value={number(metrics.jobs.counts.failed)}
				hint="{number(metrics.jobs.last_24h.failed)} in 24 h"
				tone={metrics.jobs.last_24h.failed > 0 ? 'error' : 'surface'}
			/>
			<StatTile
				label="Cancelled"
				value={number(metrics.jobs.counts.cancelled)}
				hint="{number(metrics.jobs.last_24h.cancelled)} in 24 h"
			/>
			<StatTile
				label="Cache"
				value={bytes(metrics.cache.bytes)}
				hint="{number(metrics.cache.jobs)} jobs in {metrics.cache.dir}"
			/>
		</div>
		{#if metrics.jobs.resolvers.length > 0}
			<div
				class="max-h-72 table-wrap overflow-auto rounded-container border border-surface-200-800"
			>
				<table class="table w-full">
					<thead class="sticky top-0 bg-surface-100-900"
						><tr
							><th class="px-3 py-2 text-left">Resolver</th><th class="px-3 py-2 text-right"
								>Done</th
							><th class="px-3 py-2 text-right">Failed</th><th class="px-3 py-2 text-left"
								>Last done</th
							><th class="px-3 py-2 text-left">Last failed</th></tr
						></thead
					>
					<tbody>
						{#each [...metrics.jobs.resolvers].sort((a, b) => b.done + b.failed - (a.done + a.failed)) as r (r.resolver)}
							<tr>
								<td class="px-3 py-1.5 font-medium">{r.resolver}</td>
								<td class="px-3 py-1.5 text-right tabular-nums">{number(r.done)}</td>
								<td
									class="px-3 py-1.5 text-right tabular-nums {r.failed > 0
										? 'text-error-700-300'
										: ''}">{number(r.failed)}</td
								>
								<td class="px-3 py-1.5"><RelativeTime at={r.last_done_at} /></td>
								<td class="px-3 py-1.5"><RelativeTime at={r.last_failed_at} /></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<div class="grid gap-6 lg:grid-cols-2">
		<section class="space-y-3" aria-label="Bots and storage">
			<h2 class="h6">Bots, storage and checks</h2>
			<div class="grid grid-cols-2 gap-3">
				<StatTile label="Applications" value={number(metrics.bots.applications)}>
					<div class="mt-1 flex flex-wrap gap-1">
						{#each BOT_STATES as state (state)}
							{#if metrics.bots.by_state[state]}
								<span class="inline-flex items-center gap-1 text-sm"
									><Status bot={state} /> {number(metrics.bots.by_state[state])}</span
								>
							{/if}
						{/each}
					</div>
				</StatTile>
				<StatTile
					label="Database"
					value={bytes(metrics.database.bytes)}
					hint={metrics.database.path}
				/>
				<StatTile
					label="Platform checks"
					value="{number(metrics.fixtures.passing)} / {number(metrics.fixtures.with_fixtures)}"
					hint="passing · {number(metrics.fixtures.failing)} failing · {number(
						metrics.fixtures.never
					)} never · {number(metrics.fixtures.running)} running"
					tone={metrics.fixtures.failing > 0 ? 'warning' : 'surface'}
				/>
				<StatTile
					label="Log buffer"
					value="{number(metrics.logs.buffered)} / {number(metrics.logs.capacity)}"
					hint="lines retained"
				/>
			</div>
		</section>

		<section class="space-y-3" aria-label="HTTP">
			<h2 class="h6">Outgoing HTTP</h2>
			<div class="grid grid-cols-3 gap-3">
				<StatTile label="Received" value={bytes(metrics.http.bytes_received)} />
				<StatTile label="Retries" value={number(metrics.http.retries)} />
				<StatTile label="Rate-limit waits" value={number(metrics.http.rate_limit_waits)} />
			</div>
			<div
				class="max-h-72 table-wrap overflow-auto rounded-container border border-surface-200-800"
			>
				<table class="table w-full">
					<thead class="sticky top-0 bg-surface-100-900"
						><tr
							><th class="px-3 py-2 text-left">Host</th><th class="px-3 py-2 text-right"
								>Requests</th
							><th class="px-3 py-2 text-right">Failed</th></tr
						></thead
					>
					<tbody>
						{#each requests as row (row.host)}
							<tr>
								<td class="px-3 py-1.5 font-mono text-xs">{row.host}</td>
								<td class="px-3 py-1.5 text-right tabular-nums">{number(row.total)}</td>
								<td
									class="px-3 py-1.5 text-right tabular-nums {row.failed > 0
										? 'text-error-700-300'
										: ''}">{number(row.failed)}</td
								>
							</tr>
						{:else}
							<tr
								><td colspan="3" class="px-3 py-6 text-center text-surface-600-400"
									>No requests yet.</td
								></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
		</section>
	</div>
{/if}
