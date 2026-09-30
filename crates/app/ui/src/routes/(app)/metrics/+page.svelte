<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteMap } from 'svelte/reactivity';
	import { metrics as metricsApi } from '$lib/api/endpoints';
	import type { BotState, Metrics } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
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

<PageHeader
	title="Metrics"
	description="Resource use, job counts, requests and storage. Refreshes every 5 seconds."
>
	{#if metrics}
		<p class="text-sm text-surface-600-400">Updated <RelativeTime at={metrics.at} /></p>
	{/if}
</PageHeader>

{#if error && !metrics && !loading}
	<ErrorState {error} onretry={load} />
{:else if !metrics}
	<div class="grid grid-cols-2 gap-4 md:grid-cols-4" aria-busy="true">
		{#each { length: 8 }, i (i)}<div class="h-24 placeholder animate-pulse"></div>{/each}
	</div>
{:else}
	{#if error}
		<p class="card preset-tonal-error p-3 text-sm" role="alert">
			The last refresh failed. Showing the result from <RelativeTime at={metrics.at} />.
		</p>
	{/if}

	<Card title="Process" description="Version {metrics.version} · up {span(metrics.uptime_secs)}">
		<div class="grid grid-cols-2 gap-4 md:grid-cols-4">
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
	</Card>

	<Card title="Host">
		<div class="grid grid-cols-2 gap-4 md:grid-cols-4">
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
						aria-label="Space used on {disk.mount}"
					></meter>
				</StatTile>
			{/each}
		</div>
	</Card>

	<Card title="Jobs">
		<div class="space-y-4">
			<div class="grid grid-cols-2 gap-4 md:grid-cols-3 lg:grid-cols-6">
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
				<div class="max-h-72 table-wrap overflow-auto">
					<table class="table">
						<thead class="sticky top-0 bg-surface-100-900">
							<tr>
								<th>Resolver</th>
								<th class="text-right!">Done</th>
								<th class="text-right!">Failed</th>
								<th>Last done</th>
								<th>Last failed</th>
							</tr>
						</thead>
						<tbody class="[&>tr]:hover:preset-tonal">
							{#each [...metrics.jobs.resolvers].sort((a, b) => b.done + b.failed - (a.done + a.failed)) as r (r.resolver)}
								<tr>
									<td class="font-medium">{r.resolver}</td>
									<td class="text-right tabular-nums">{number(r.done)}</td>
									<td class="text-right tabular-nums {r.failed > 0 ? 'text-error-600-400' : ''}">
										{number(r.failed)}
									</td>
									<td><RelativeTime at={r.last_done_at} /></td>
									<td><RelativeTime at={r.last_failed_at} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</div>
	</Card>

	<div class="grid items-start gap-6 lg:grid-cols-2">
		<Card title="Bots and storage">
			<div class="grid grid-cols-2 gap-4">
				<StatTile label="Applications" value={number(metrics.bots.applications)}>
					<div class="mt-1 flex flex-wrap gap-1.5">
						{#each BOT_STATES as state (state)}
							{#if metrics.bots.by_state[state]}
								<span class="inline-flex items-center gap-1 text-sm">
									<Status bot={state} />
									{number(metrics.bots.by_state[state])}
								</span>
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
				<StatTile
					label="Retention"
					value={metrics.retention.last
						? `${number(metrics.retention.last.jobs_removed + metrics.retention.last.failed_removed)} jobs`
						: EMPTY}
					hint={metrics.retention.last
						? `${bytes(metrics.retention.last.bytes_freed)} freed in the last sweep · ${number(metrics.retention.sweeps)} sweeps`
						: 'No sweep yet'}
					tone={metrics.retention.last?.error ? 'warning' : 'surface'}
				/>
				<StatTile
					label="Backups"
					value={metrics.backups.enabled ? number(metrics.backups.count) : 'Off'}
					hint={metrics.backups.last_error ??
						(metrics.backups.newest_at
							? `${bytes(metrics.backups.bytes)} kept · newest ${new Date(metrics.backups.newest_at).toLocaleString()}`
							: 'No backup yet')}
					tone={metrics.backups.last_error ? 'error' : 'surface'}
				/>
				<StatTile
					label="Video encoder"
					value={metrics.transcode.h264_encoder ?? EMPTY}
					hint={metrics.transcode.shortfall ??
						`${metrics.transcode.hardware ?? 'software'} · ${metrics.transcode.source} ffmpeg · ${metrics.transcode.choice} chosen`}
					tone={metrics.transcode.shortfall ? 'error' : 'surface'}
				/>
			</div>
		</Card>

		<Card title="Outgoing HTTP">
			<div class="space-y-4">
				<div class="grid grid-cols-3 gap-4">
					<StatTile label="Received" value={bytes(metrics.http.bytes_received)} />
					<StatTile label="Retries" value={number(metrics.http.retries)} />
					<StatTile label="Rate-limit waits" value={number(metrics.http.rate_limit_waits)} />
				</div>
				<div class="max-h-72 table-wrap overflow-auto">
					<table class="table">
						<thead class="sticky top-0 bg-surface-100-900">
							<tr>
								<th>Host</th>
								<th class="text-right!">Requests</th>
								<th class="text-right!">Failed</th>
							</tr>
						</thead>
						<tbody class="[&>tr]:hover:preset-tonal">
							{#each requests as row (row.host)}
								<tr>
									<td class="font-mono text-xs">{row.host}</td>
									<td class="text-right tabular-nums">{number(row.total)}</td>
									<td class="text-right tabular-nums {row.failed > 0 ? 'text-error-600-400' : ''}">
										{number(row.failed)}
									</td>
								</tr>
							{:else}
								<tr>
									<td colspan="3" class="py-6 text-center text-surface-600-400">No requests yet.</td
									>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</div>
		</Card>
	</div>
{/if}
