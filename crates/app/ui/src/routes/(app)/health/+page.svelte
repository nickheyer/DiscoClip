<script lang="ts">
	import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw';
	import { onMount } from 'svelte';
	import { health as healthApi } from '$lib/api/endpoints';
	import type { Health, HealthCheck } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { number, span } from '$lib/format';

	const EVERY = 15_000;

	let health = $state<Health | null>(null);
	let loading = $state(true);
	let refreshing = $state(false);
	let error = $state<unknown>(null);

	async function load(quiet = false) {
		if (quiet) refreshing = true;
		else loading = true;
		error = null;
		try {
			health = await healthApi.get();
		} catch (err) {
			error = err;
		} finally {
			loading = false;
			refreshing = false;
		}
	}

	onMount(() => {
		void load();
		const timer = setInterval(() => void load(true), EVERY);
		return () => clearInterval(timer);
	});

	/** The order checks read in: what needs attention first. */
	const RANK = { fail: 0, warn: 1, ok: 2 };
	const checks = $derived(
		[...(health?.checks ?? [])].sort((a, b) => RANK[a.status] - RANK[b.status])
	);
	const attention = $derived(checks.filter((check) => check.status !== 'ok').length);

	const columns: Column<HealthCheck>[] = [
		{ key: 'label', label: 'Check', value: (check) => check.label, class: 'font-medium' },
		{ key: 'status', label: 'Status', cell: statusCell },
		{ key: 'detail', label: 'Detail', value: (check) => check.detail, class: 'text-sm' }
	];
</script>

{#snippet statusCell(check: HealthCheck)}
	<Status health={check.status} />
{/snippet}

<PageHeader title="Health" description="Checks refresh every 15 seconds.">
	{#if health}
		<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-surface-600-400">
			<Status health={health.status} />
			<span>Version {health.version}</span>
			<span>Up {span(health.uptime_secs)}, started <RelativeTime at={health.started_at} /></span>
			<span>Checked <RelativeTime at={health.at} /></span>
		</div>
	{/if}
	{#snippet actions()}
		<button type="button" class="btn preset-tonal" onclick={() => load(true)} disabled={refreshing}>
			{#if refreshing}<Spinner />{:else}<RefreshCwIcon class="size-4" />{/if}
			Refresh
		</button>
	{/snippet}
</PageHeader>

{#if error && !loading && !health}
	<ErrorState {error} onretry={() => load()} />
{:else}
	{#if error && health}
		<p class="card preset-tonal-error p-3 text-sm" role="alert">
			The last refresh failed. Showing the result from <RelativeTime at={health.at} />.
		</p>
	{/if}
	<Card
		title="Checks"
		count={health
			? attention > 0
				? `${number(attention)} of ${number(checks.length)} need attention`
				: `${number(checks.length)} passing`
			: undefined}
		flush
	>
		<DataTable
			rows={checks}
			{columns}
			rowKey={(check) => check.name}
			loading={loading && !health}
			placeholderRows={6}
			flush
			class="p-2"
		>
			{#snippet empty()}
				The server reports no checks.
			{/snippet}
		</DataTable>
	</Card>
{/if}
