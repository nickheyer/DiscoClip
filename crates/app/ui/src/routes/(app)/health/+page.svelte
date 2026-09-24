<script lang="ts">
	import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw';
	import { onMount } from 'svelte';
	import { health as healthApi } from '$lib/api/endpoints';
	import type { Health } from '$lib/api/types';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { absolute, span } from '$lib/format';

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

	const TONE = {
		ok: 'border-success-500/40',
		warn: 'border-warning-500/60',
		fail: 'border-error-500/60'
	} as const;
</script>

<PageHeader title="Health" />

<Toolbar description="Checks refresh every 15 seconds.">
	{#if health}<Status health={health.status} class="mr-auto" />{/if}
	<button type="button" class="btn preset-tonal" onclick={() => load(true)} disabled={refreshing}>
		{#if refreshing}<Spinner />{:else}<RefreshCwIcon class="size-4" />{/if}
		Refresh
	</button>
</Toolbar>

{#if error && !loading && !health}
	<ErrorState {error} onretry={() => load()} />
{:else if !health}
	<div class="grid gap-4 md:grid-cols-2" aria-busy="true">
		{#each { length: 6 }, i (i)}<div class="h-24 placeholder animate-pulse"></div>{/each}
	</div>
{:else}
	{#if error}
		<p class="card preset-tonal-error p-3 text-sm" role="alert">
			The last refresh failed. Showing the result from <RelativeTime at={health.at} />.
		</p>
	{/if}
	<section
		class="card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
		aria-label="Server"
	>
		<KeyValue class="sm:grid-cols-[auto_1fr_auto_1fr]">
			<KeyValueRow label="Version" value={health.version} />
			<KeyValueRow label="Started" value={absolute(health.started_at)} />
			<KeyValueRow label="Uptime" value={span(health.uptime_secs)} />
			<KeyValueRow label="Checked"><RelativeTime at={health.at} /></KeyValueRow>
		</KeyValue>
	</section>
	<div class="grid gap-4 md:grid-cols-2">
		{#each health.checks as check (check.name)}
			<section
				class="space-y-2 card border bg-surface-100-900 p-5 sm:p-6 {TONE[check.status]}"
				aria-label={check.label}
			>
				<div class="flex items-center justify-between gap-2">
					<h2 class="font-semibold">{check.label}</h2>
					<Status health={check.status} />
				</div>
				<p class="text-sm text-surface-600-400">{check.detail}</p>
			</section>
		{/each}
	</div>
{/if}
