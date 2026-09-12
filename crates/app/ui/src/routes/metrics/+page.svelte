<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import Button from '$lib/components/Button.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatBytes, formatNumber, pluralize } from '$lib/format';
	import { formatUptime, percent } from '$lib/health';

	let { data }: { data: PageData } = $props();

	const m = $derived(data.metrics);
	let live = $state(true);

	$effect(() => {
		if (!live) return;
		const timer = setInterval(() => void invalidate('app:metrics'), 5000);
		return () => clearInterval(timer);
	});

	const memoryUsed = $derived(m.system.total_memory_bytes - m.system.available_memory_bytes);
	const requestsTotal = $derived(m.http.requests.reduce((sum, r) => sum + r.count, 0));
	const hosts = $derived.by(() => {
		const byHost = new Map<string, { host: string; total: number; ok: number; errors: number; statuses: string[] }>();
		for (const r of m.http.requests) {
			let row = byHost.get(r.host);
			if (!row) {
				row = { host: r.host, total: 0, ok: 0, errors: 0, statuses: [] };
				byHost.set(r.host, row);
			}
			row.total += r.count;
			if (r.status < 400) row.ok += r.count;
			else row.errors += r.count;
			row.statuses.push(`${r.status} ×${formatNumber(r.count)}`);
		}
		return [...byHost.values()].sort((a, b) => b.total - a.total);
	});
	const botStates = $derived(Object.entries(m.bots.by_state).sort((a, b) => b[1] - a[1]));
	const BOT_LABELS: Record<string, string> = {
		connected: 'connected',
		starting: 'starting',
		retrying: 'retrying',
		stopped: 'stopped',
		disabled: 'without a token',
		failed: 'failed'
	};
</script>

<svelte:head>
	<title>Metrics · DiscoClip</title>
</svelte:head>

<PageHeader title="Metrics" description="What the server is doing, in numbers, read again every five seconds.">
	{#snippet meta()}
		<span class="faint small">Read <Time value={m.at} /> · version {m.version} · up {formatUptime(m.uptime_secs)}</span>
	{/snippet}
	{#snippet actions()}
		<Button icon={live ? 'stop' : 'play'} onclick={() => (live = !live)}>{live ? 'Pause' : 'Resume'}</Button>
		<Button href="/health" icon="check-circle">Health</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="stack">
		<div class="section-title"><h2>Process and machine</h2></div>
		<div class="tiles">
			{#if m.process}
				<div class="tile">
					<span class="tile-value">{formatBytes(m.process.rss_bytes)}</span>
					<span class="tile-label">resident memory</span>
					<span class="tile-sub">{formatBytes(m.process.virtual_bytes)} virtual · pid {m.process.pid}</span>
				</div>
				<div class="tile">
					<span class="tile-value">{m.process.cpu_percent.toFixed(1)}%</span>
					<span class="tile-label">CPU, of one core</span>
					<span class="tile-sub">since the previous reading</span>
				</div>
			{:else}
				<div class="tile">
					<span class="tile-value">—</span>
					<span class="tile-label">process</span>
					<span class="tile-sub">the operating system does not list this process</span>
				</div>
			{/if}
			<div class="tile">
				<span class="tile-value">{formatBytes(memoryUsed)}</span>
				<span class="tile-label">machine memory in use</span>
				<span class="tile-sub">{percent(memoryUsed, m.system.total_memory_bytes)}% of {formatBytes(m.system.total_memory_bytes)}</span>
			</div>
			<div class="tile">
				<span class="tile-value">{m.system.load_average[0].toFixed(2)}</span>
				<span class="tile-label">load, one minute</span>
				<span class="tile-sub">{m.system.load_average[1].toFixed(2)} five · {m.system.load_average[2].toFixed(2)} fifteen · {pluralize(m.system.cpus, 'CPU')}</span>
			</div>
		</div>
		{#if m.system.disks.length}
			<div class="table-wrap">
				<table class="table">
					<thead><tr><th>Disk</th><th>Holds</th><th>Free</th><th>Size</th><th>Used</th></tr></thead>
					<tbody>
						{#each m.system.disks as disk (disk.mount)}
							{@const used = disk.total_bytes - disk.available_bytes}
							<tr>
								<td><code>{disk.mount}</code></td>
								<td>{disk.holds.join(', ')}</td>
								<td class="num">{formatBytes(disk.available_bytes)}</td>
								<td class="num">{formatBytes(disk.total_bytes)}</td>
								<td>
									<div class="meter" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent(used, disk.total_bytes)} aria-label={`${disk.mount} used`}>
										<div class={['meter-fill', percent(used, disk.total_bytes) >= 90 && 'full']} style={`width:${percent(used, disk.total_bytes)}%`}></div>
									</div>
									<span class="small faint">{percent(used, disk.total_bytes)}%</span>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="stack">
		<div class="section-title"><h2>Jobs</h2><a href="/jobs" class="small">All jobs</a></div>
		<div class="tiles">
			<div class="tile"><span class="tile-value">{formatNumber(m.jobs.counts.queued)}</span><span class="tile-label">queued</span></div>
			<div class="tile"><span class="tile-value">{formatNumber(m.jobs.counts.running)}</span><span class="tile-label">running</span><span class="tile-sub">{m.jobs.utilisation.active} of {pluralize(m.jobs.utilisation.workers, 'worker')} busy · {m.jobs.utilisation.waiting} waiting</span></div>
			<div class="tile"><span class="tile-value">{formatNumber(m.jobs.counts.done)}</span><span class="tile-label">done</span><span class="tile-sub">{formatNumber(m.jobs.last_24h.done)} in the last day</span></div>
			<div class="tile"><span class="tile-value">{formatNumber(m.jobs.counts.failed)}</span><span class="tile-label">failed</span><span class="tile-sub">{formatNumber(m.jobs.last_24h.failed)} in the last day · {formatNumber(m.jobs.counts.cancelled)} cancelled</span></div>
		</div>
		{#if m.jobs.resolvers.length}
			<div class="table-wrap">
				<table class="table">
					<thead><tr><th>Resolver</th><th>Done</th><th>Failed</th><th>Last done</th><th>Last failed</th></tr></thead>
					<tbody>
						{#each m.jobs.resolvers as resolver (resolver.resolver)}
							<tr>
								<td><code>{resolver.resolver}</code></td>
								<td class="num">{formatNumber(resolver.done)}</td>
								<td class="num">{formatNumber(resolver.failed)}</td>
								<td><Time value={resolver.last_done_at} /></td>
								<td><Time value={resolver.last_failed_at} /></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="stack">
		<div class="section-title"><h2>HTTP client</h2><span class="faint small">Since the server started</span></div>
		<div class="tiles">
			<div class="tile"><span class="tile-value">{formatNumber(requestsTotal)}</span><span class="tile-label">requests</span><span class="tile-sub">to {pluralize(hosts.length, 'host')}</span></div>
			<div class="tile"><span class="tile-value">{formatBytes(m.http.bytes_received)}</span><span class="tile-label">received</span></div>
			<div class="tile"><span class="tile-value">{formatNumber(m.http.retries)}</span><span class="tile-label">retries</span></div>
			<div class="tile"><span class="tile-value">{formatNumber(m.http.rate_limit_waits)}</span><span class="tile-label">rate limit waits</span></div>
		</div>
		{#if hosts.length}
			<div class="table-wrap">
				<table class="table">
					<thead><tr><th>Host</th><th>Requests</th><th>2xx and 3xx</th><th>4xx and 5xx</th><th>By status</th></tr></thead>
					<tbody>
						{#each hosts as row (row.host)}
							<tr>
								<td><code>{row.host}</code></td>
								<td class="num">{formatNumber(row.total)}</td>
								<td class="num">{formatNumber(row.ok)}</td>
								<td class={['num', row.errors > 0 && 'errors']}>{formatNumber(row.errors)}</td>
								<td><div class="chips">{#each row.statuses as status (status)}<span class="chip">{status}</span>{/each}</div></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="faint small">No request has been made yet.</p>
		{/if}
	</section>

	<section class="stack">
		<div class="section-title"><h2>Bots, storage, fixtures and log</h2></div>
		<div class="tiles">
			<div class="tile">
				<span class="tile-value">{formatNumber(m.bots.applications)}</span>
				<span class="tile-label">{m.bots.applications === 1 ? 'Discord application' : 'Discord applications'}</span>
				<span class="tile-sub">{botStates.length ? botStates.map(([state, n]) => `${n} ${BOT_LABELS[state] ?? state}`).join(' · ') : 'none added'}</span>
			</div>
			<div class="tile">
				<span class="tile-value">{formatBytes(m.cache.bytes)}</span>
				<span class="tile-label">cache</span>
				<span class="tile-sub">{pluralize(m.cache.jobs, 'job directory', 'job directories')} in {m.cache.dir}</span>
			</div>
			<div class="tile">
				<span class="tile-value">{formatBytes(m.database.bytes)}</span>
				<span class="tile-label">database</span>
				<span class="tile-sub">{m.database.path}</span>
			</div>
			<div class="tile">
				<span class="tile-value">{m.fixtures.passing} / {m.fixtures.with_fixtures}</span>
				<span class="tile-label">platforms passing</span>
				<span class="tile-sub">{m.fixtures.failing} failing · {m.fixtures.never} not run{m.fixtures.running ? ` · ${m.fixtures.running} running` : ''} · <a href="/platforms">platforms</a></span>
			</div>
			<div class="tile">
				<span class="tile-value">{formatNumber(m.logs.buffered)}</span>
				<span class="tile-label">log lines kept</span>
				<span class="tile-sub">of {formatNumber(m.logs.capacity)} · <a href="/logs">log</a></span>
			</div>
		</div>
	</section>
</div>

<style>
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 12px;
	}

	.tile {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 14px 16px;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		box-shadow: var(--shadow-sm);
		min-width: 0;
	}

	.tile-value {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
		font-variant-numeric: tabular-nums;
	}

	.tile-label {
		font-size: 12.5px;
		color: var(--text-3);
	}

	.tile-sub {
		font-size: 12px;
		color: var(--text-3);
		overflow-wrap: anywhere;
	}

	.meter {
		height: 6px;
		border-radius: 3px;
		background: var(--surface-3);
		overflow: hidden;
		min-width: 80px;
	}

	.meter-fill {
		height: 100%;
		background: var(--accent);
		border-radius: 3px;
	}

	.meter-fill.full {
		background: var(--danger);
	}

	td.errors {
		color: var(--danger-text);
	}
</style>
