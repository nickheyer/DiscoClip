<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { HEALTH_LABELS, formatUptime, healthIcon, healthTone } from '$lib/health';

	let { data }: { data: PageData } = $props();

	const health = $derived(data.health);
	const failing = $derived(health.checks.filter((c) => c.status === 'fail').length);
	const warning = $derived(health.checks.filter((c) => c.status === 'warn').length);
	let refreshing = $state(false);

	const summary = $derived(
		health.status === 'ok'
			? 'Every part is working.'
			: health.status === 'warn'
				? `${warning} ${warning === 1 ? 'part needs' : 'parts need'} a look.`
				: `${failing} ${failing === 1 ? 'part is' : 'parts are'} failing.`
	);

	// The page asks again every ten seconds while it is open.
	$effect(() => {
		const timer = setInterval(() => void invalidate('app:health'), 10_000);
		return () => clearInterval(timer);
	});

	async function refresh() {
		refreshing = true;
		try {
			await invalidate('app:health');
		} finally {
			refreshing = false;
		}
	}
</script>

<svelte:head>
	<title>Health · DiscoClip</title>
</svelte:head>

<PageHeader title="Health" description="Whether each part the server runs on is working.">
	{#snippet actions()}
		<Button icon="refresh" loading={refreshing} onclick={refresh}>Check again</Button>
		<Button href="/metrics" icon="activity">Metrics</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class={['overall', `overall-${healthTone(health.status)}`]} aria-live="polite">
		<span class="overall-icon"><Icon name={healthIcon(health.status)} size={28} /></span>
		<div class="overall-text">
			<h2>{HEALTH_LABELS[health.status]}</h2>
			<p class="muted">{summary}</p>
		</div>
		<dl class="kv overall-facts">
			<dt>Version</dt>
			<dd>{health.version}</dd>
			<dt>Up for</dt>
			<dd>{formatUptime(health.uptime_secs)}</dd>
			<dt>Started</dt>
			<dd><Time value={health.started_at} mode="absolute" /></dd>
			<dt>Checked</dt>
			<dd><Time value={health.at} /></dd>
		</dl>
	</section>

	<div class="grid-3">
		{#each health.checks as check (check.name)}
			<article class={['card check', `check-${check.status}`]}>
				<div class="check-head">
					<span class={['check-icon', `tone-${healthTone(check.status)}`]}><Icon name={healthIcon(check.status)} size={18} /></span>
					<h3>{check.label}</h3>
					<Badge tone={healthTone(check.status)} size="sm">{HEALTH_LABELS[check.status]}</Badge>
				</div>
				<p class="detail">{check.detail}</p>
			</article>
		{/each}
	</div>
</div>

<style>
	.overall {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto;
		align-items: center;
		gap: 18px;
		padding: 20px 24px;
		border: 1px solid var(--border);
		border-radius: var(--radius);
		background: var(--surface);
		box-shadow: var(--shadow-sm);
	}

	.overall-ok {
		border-color: color-mix(in srgb, var(--ok) 40%, var(--border));
	}

	.overall-warn {
		border-color: color-mix(in srgb, var(--warn) 50%, var(--border));
	}

	.overall-danger {
		border-color: color-mix(in srgb, var(--danger) 50%, var(--border));
	}

	.overall-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 52px;
		height: 52px;
		border-radius: 14px;
	}

	.overall-ok .overall-icon {
		background: var(--ok-soft);
		color: var(--ok-text);
	}

	.overall-warn .overall-icon {
		background: var(--warn-soft);
		color: var(--warn-text);
	}

	.overall-danger .overall-icon {
		background: var(--danger-soft);
		color: var(--danger-text);
	}

	.overall-text h2 {
		font-size: 20px;
	}

	.overall-facts {
		grid-template-columns: max-content max-content;
		gap: 4px 14px;
	}

	.check {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px 18px;
	}

	.check-head {
		display: flex;
		align-items: center;
		gap: 10px;
	}

	.check-head h3 {
		flex: 1;
		min-width: 0;
	}

	.check-icon {
		display: flex;
	}

	.tone-ok {
		color: var(--ok-text);
	}

	.tone-warn {
		color: var(--warn-text);
	}

	.tone-danger {
		color: var(--danger-text);
	}

	.detail {
		color: var(--text-2);
		overflow-wrap: anywhere;
	}

	@media (max-width: 720px) {
		.overall {
			grid-template-columns: auto minmax(0, 1fr);
		}

		.overall-facts {
			grid-column: 1 / 3;
		}
	}
</style>
