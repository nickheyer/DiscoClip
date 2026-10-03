<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteMap } from 'svelte/reactivity';
	import { metrics as metricsApi } from '$lib/api/endpoints';
	import type { BotState, Metrics } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import StatTile from '$lib/components/StatTile.svelte';
	import Status from '$lib/components/Status.svelte';
	import Bytes from '$lib/components/Bytes.svelte';
	import Count from '$lib/components/Count.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import { number, percent } from '$lib/format';

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
		<p class="text-sm text-surface-600-400">Updated <Timestamp at={metrics.at} /></p>
	{/if}
</PageHeader>

{#if error && !metrics && !loading}
	<ErrorState {error} onretry={load} />
{:else if !metrics}
	<div class="grid grid-cols-2 gap-4 md:grid-cols-4" aria-busy="true">
		{#each { length: 8 }, i (i)}<div class="h-24 placeholder animate-pulse"></div>{/each}
	</div>
{:else}
	{@const m = metrics}
	{#if error}
		<p class="card preset-tonal-error p-3 text-sm" role="alert">
			The last refresh failed. Showing the result from <Timestamp at={m.at} />.
		</p>
	{/if}

	<Card title="Process">
		<KeyValue class="mb-4">
			<KeyValueRow label="Version" value={m.version} mono />
			<KeyValueRow label="Uptime"><Duration value={m.uptime_secs} /></KeyValueRow>
		</KeyValue>
		<div class="grid grid-cols-2 gap-4 md:grid-cols-4">
			{#if m.process}
				{@const process = m.process}
				<StatTile label="CPU" value={percent(process.cpu_percent / 100, 1)} hint="of one core" />
				<StatTile label="Memory">
					{#snippet figure()}<Bytes value={process.rss_bytes} />{/snippet}
					{#snippet hint()}resident · <Bytes value={process.virtual_bytes} /> virtual{/snippet}
				</StatTile>
				<StatTile label="PID" value={process.pid}>
					{#snippet hint()}running <Duration value={process.run_time_secs} />{/snippet}
				</StatTile>
			{/if}
			<StatTile
				label="Load"
				value={m.system.load_average.map((l) => l.toFixed(2)).join(' / ')}
				hint="1, 5 and 15 minutes · {number(m.system.cpus)} CPUs"
			/>
		</div>
	</Card>

	<Card title="Host">
		<div class="grid grid-cols-2 gap-4 md:grid-cols-4">
			<StatTile
				label="Memory free"
				tone={m.system.available_memory_bytes / m.system.total_memory_bytes < 0.1
					? 'warning'
					: 'surface'}
			>
				{#snippet figure()}<Bytes value={m.system.available_memory_bytes} />{/snippet}
				{#snippet hint()}of <Bytes value={m.system.total_memory_bytes} />{/snippet}
			</StatTile>
			{#each m.system.disks as disk (disk.mount)}
				{@const used = disk.total_bytes - disk.available_bytes}
				{@const ratio = disk.total_bytes > 0 ? used / disk.total_bytes : 0}
				<StatTile
					label={disk.mount}
					tone={ratio > 0.9 ? 'error' : ratio > 0.8 ? 'warning' : 'surface'}
				>
					{#snippet figure()}<Bytes value={disk.available_bytes} />{/snippet}
					{#snippet hint()}free of <Bytes value={disk.total_bytes} /> · holds {disk.holds.join(
							', '
						)}{/snippet}
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
					value={number(m.jobs.counts.queued)}
					hint="{number(m.jobs.queue_depth)} waiting"
				/>
				<StatTile
					label="Running"
					value={number(m.jobs.counts.running)}
					hint="{number(m.jobs.utilisation.active)} of {number(m.jobs.utilisation.workers)} workers"
					tone="primary"
				/>
				<StatTile
					label="Done"
					value={number(m.jobs.counts.done)}
					hint="{number(m.jobs.last_24h.done)} in 24 h"
					tone="success"
				/>
				<StatTile
					label="Failed"
					value={number(m.jobs.counts.failed)}
					hint="{number(m.jobs.last_24h.failed)} in 24 h"
					tone={m.jobs.last_24h.failed > 0 ? 'error' : 'surface'}
				/>
				<StatTile
					label="Cancelled"
					value={number(m.jobs.counts.cancelled)}
					hint="{number(m.jobs.last_24h.cancelled)} in 24 h"
				/>
				<StatTile label="Cache">
					{#snippet figure()}<Bytes value={m.cache.bytes} />{/snippet}
					{#snippet hint()}<Count value={m.cache.jobs} noun="job" /> in {m.cache.dir}{/snippet}
				</StatTile>
			</div>
			{#if m.jobs.resolvers.length > 0}
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
							{#each [...m.jobs.resolvers].sort((a, b) => b.done + b.failed - (a.done + a.failed)) as r (r.resolver)}
								<tr>
									<td class="font-medium">{r.resolver}</td>
									<td class="text-right tabular-nums">{number(r.done)}</td>
									<td class="text-right tabular-nums {r.failed > 0 ? 'text-error-600-400' : ''}">
										{number(r.failed)}
									</td>
									<td><Timestamp at={r.last_done_at} /></td>
									<td><Timestamp at={r.last_failed_at} /></td>
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
				<StatTile label="Applications" value={number(m.bots.applications)}>
					<div class="mt-1 flex flex-wrap gap-1.5">
						{#each BOT_STATES as state (state)}
							{#if m.bots.by_state[state]}
								<span class="inline-flex items-center gap-1 text-sm">
									<Status bot={state} />
									{number(m.bots.by_state[state])}
								</span>
							{/if}
						{/each}
					</div>
				</StatTile>
				<StatTile label="Database" hint={m.database.path}>
					{#snippet figure()}<Bytes value={m.database.bytes} />{/snippet}
				</StatTile>
				<StatTile
					label="Platform checks"
					value="{number(m.fixtures.working)} / {number(m.fixtures.platforms)}"
					hint="working · {number(m.fixtures.failing)} failing · {number(
						m.fixtures.login_required
					)} need a login · {number(m.fixtures.unknown)} not checked · {number(
						m.fixtures.running
					)} running"
					tone={m.fixtures.failing > 0 ? 'warning' : 'surface'}
				/>
				<StatTile
					label="Log buffer"
					value="{number(m.logs.buffered)} / {number(m.logs.capacity)}"
					hint="lines retained"
				/>
				<StatTile label="Retention" tone={m.retention.last?.error ? 'warning' : 'surface'}>
					{#snippet figure()}
						{#if m.retention.last}
							<Count
								value={m.retention.last.jobs_removed + m.retention.last.failed_removed}
								noun="job"
							/>
						{/if}
					{/snippet}
					{#snippet hint()}
						{#if m.retention.last}
							<Bytes value={m.retention.last.bytes_freed} /> freed in the last sweep ·
							<Count value={m.retention.sweeps} noun="sweep" />
						{:else}
							No sweep yet
						{/if}
					{/snippet}
				</StatTile>
				<StatTile
					label="Backups"
					value={m.backups.enabled ? number(m.backups.count) : 'Off'}
					tone={m.backups.last_error ? 'error' : 'surface'}
				>
					{#snippet hint()}
						{#if m.backups.last_error}
							{m.backups.last_error}
						{:else if m.backups.newest_at}
							<Bytes value={m.backups.bytes} /> kept · newest
							<Timestamp at={m.backups.newest_at} />
						{:else}
							No backup yet
						{/if}
					{/snippet}
				</StatTile>
				<StatTile
					label="Video encoder"
					value={m.transcode.h264_encoder ?? ''}
					hint={m.transcode.shortfall ??
						`${m.transcode.hardware ?? 'software'} · ${m.transcode.source} ffmpeg · ${m.transcode.choice} chosen`}
					tone={m.transcode.shortfall ? 'error' : 'surface'}
				/>
			</div>
		</Card>

		<Card title="Outgoing HTTP">
			<div class="space-y-4">
				<div class="grid grid-cols-3 gap-4">
					<StatTile label="Received">
						{#snippet figure()}<Bytes value={m.http.bytes_received} />{/snippet}
					</StatTile>
					<StatTile label="Retries" value={number(m.http.retries)} />
					<StatTile label="Rate-limit waits" value={number(m.http.rate_limit_waits)} />
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
