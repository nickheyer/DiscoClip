<script lang="ts">
	import type { Entry } from '$lib/api';
	import { ACTION_LABELS, TARGET_KIND_LABELS, actionTone, targetHref } from '$lib/audit';
	import Badge from './Badge.svelte';
	import DetailsView from './DetailsView.svelte';
	import Icon from './Icon.svelte';
	import Time from './Time.svelte';

	let { entry, compact = false }: { entry: Entry; compact?: boolean } = $props();

	let open = $state(false);
	const href = $derived(targetHref(entry.target));
	const targetLabel = $derived(entry.target.name ?? entry.target.id);
	const hasDetails = $derived(Object.keys(entry.details ?? {}).length > 0);
</script>

<article class={['entry', compact && 'compact']}>
	<div class="line">
		<Badge tone={actionTone(entry.action)} size="sm">{ACTION_LABELS[entry.action]}</Badge>
		<span class="who">
			{#if entry.actor.kind === 'user'}
				<a href={`/users/${encodeURIComponent(entry.actor.id)}`} class="strong">{entry.actor.username}</a>
				<span class="faint small">
					via {entry.actor.via === 'session' ? 'the app' : 'an API token'} · {entry.actor.ip}
				</span>
			{:else}
				<span class="strong">Provisioning</span>
				{#if entry.actor.file}<span class="faint small mono">{entry.actor.file}</span>{/if}
			{/if}
		</span>
		<span class="target">
			<span class="faint small">{TARGET_KIND_LABELS[entry.target.kind]}</span>
			{#if href}
				<a {href}>{targetLabel}</a>
			{:else}
				<code>{targetLabel}</code>
			{/if}
		</span>
		<span class="when faint small"><Time value={entry.at} /></span>
		{#if hasDetails}
			<button
				type="button"
				class="toggle"
				onclick={() => (open = !open)}
				aria-expanded={open}
				aria-label={open ? 'Hide details' : 'Show details'}
			>
				<Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} />
			</button>
		{/if}
	</div>
	{#if open}
		<div class="body"><DetailsView details={entry.details} /></div>
	{/if}
</article>

<style>
	.entry {
		padding: 10px 14px;
		border-bottom: 1px solid var(--border);
	}

	.entry:last-child {
		border-bottom: none;
	}

	.line {
		display: grid;
		grid-template-columns: max-content minmax(140px, 1.2fr) minmax(140px, 1fr) max-content max-content;
		align-items: center;
		gap: 12px;
	}

	.who,
	.target {
		display: flex;
		align-items: baseline;
		gap: 6px;
		flex-wrap: wrap;
		min-width: 0;
	}

	.target code {
		overflow-wrap: anywhere;
	}

	.toggle {
		display: flex;
		padding: 4px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
	}

	.toggle:hover {
		background: var(--surface-3);
		color: var(--text);
	}

	.body {
		margin-top: 10px;
		padding: 12px;
		border-radius: var(--radius-sm);
		background: var(--surface-2);
	}

	/* In a narrow card the row wraps: the action and time first, then who did what to what. */
	.compact .line {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
	}

	.compact .when {
		order: 1;
		margin-left: auto;
	}

	.compact .toggle {
		order: 2;
	}

	.compact .who {
		order: 3;
		flex: 1 1 100%;
	}

	.compact .target {
		order: 4;
		flex: 1 1 100%;
	}

	@media (max-width: 760px) {
		.line {
			display: flex;
			flex-wrap: wrap;
			gap: 4px 10px;
		}

		.when {
			order: 1;
			margin-left: auto;
		}

		.toggle {
			order: 2;
		}

		.who {
			order: 3;
			flex: 1 1 100%;
		}

		.target {
			order: 4;
			flex: 1 1 100%;
		}
	}
</style>
