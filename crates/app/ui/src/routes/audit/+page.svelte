<script lang="ts">
	import { goto } from '$app/navigation';
	import type { PageData } from './$types';
	import { ACTIONS, TARGET_KINDS, audit, messageOf } from '$lib/api';
	import type { Action, Entry, TargetKind } from '$lib/api';
	import { ACTION_LABELS, TARGET_KIND_LABELS } from '$lib/audit';
	import Alert from '$lib/components/Alert.svelte';
	import AuditEntry from '$lib/components/AuditEntry.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import { pluralize } from '$lib/format';
	import { LIMITS } from './+page';

	let { data }: { data: PageData } = $props();

	// Filters, bound to the address so a view can be shared and returned to.

	let actor = $state('');
	let action = $state<Action | ''>('');
	let targetKind = $state<TargetKind | ''>('');
	let targetId = $state('');
	let since = $state('');
	let until = $state('');
	let limit = $state(50);

	function toLocalInput(iso: string | undefined): string {
		if (!iso) return '';
		const date = new Date(iso);
		if (Number.isNaN(date.getTime())) return '';
		const pad = (n: number) => String(n).padStart(2, '0');
		return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
	}

	function fromLocalInput(value: string): string | undefined {
		if (!value) return undefined;
		const date = new Date(value);
		return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
	}

	$effect.pre(() => {
		actor = data.query.actor ?? '';
		action = data.query.action ?? '';
		targetKind = data.query.target_kind ?? '';
		targetId = data.query.target_id ?? '';
		since = toLocalInput(data.query.since);
		until = toLocalInput(data.query.until);
		limit = data.query.limit ?? 50;
	});

	const rangeProblem = $derived(
		since && until && new Date(since).getTime() > new Date(until).getTime()
			? 'The start is after the end.'
			: null
	);
	const filtered = $derived(
		Boolean(data.query.actor || data.query.action || data.query.target_kind || data.query.since || data.query.until)
	);

	function apply(event: SubmitEvent) {
		event.preventDefault();
		if (rangeProblem) return;
		const params = new URLSearchParams();
		if (actor) params.set('actor', actor);
		if (action) params.set('action', action);
		if (targetKind) {
			params.set('target_kind', targetKind);
			if (targetId.trim()) params.set('target_id', targetId.trim());
		}
		const sinceIso = fromLocalInput(since);
		const untilIso = fromLocalInput(until);
		if (sinceIso) params.set('since', sinceIso);
		if (untilIso) params.set('until', untilIso);
		if (limit !== 50) params.set('limit', String(limit));
		const text = params.toString();
		void goto(text ? `/audit?${text}` : '/audit', { keepFocus: true });
	}

	function clear() {
		void goto('/audit');
	}

	// Paging: pages after the first are appended in place.

	let extra = $state<Entry[]>([]);
	let next = $state<string | null>(null);
	let loadingMore = $state(false);
	let moreError = $state<string | null>(null);
	$effect.pre(() => {
		extra = [];
		next = data.page.next;
		moreError = null;
	});
	const entries = $derived([...data.page.entries, ...extra]);

	async function loadMore() {
		if (!next) return;
		loadingMore = true;
		moreError = null;
		try {
			const page = await audit.list({ ...data.query, before: next });
			extra = [...extra, ...page.entries];
			next = page.next;
		} catch (cause) {
			moreError = messageOf(cause);
		} finally {
			loadingMore = false;
		}
	}
</script>

<svelte:head>
	<title>Audit log · DiscoClip</title>
</svelte:head>

<PageHeader title="Audit log" description="Every settings change and Discord management action, newest first, with who did it and what changed." />

<div class="stack">
	<form class="card filters" onsubmit={apply}>
		<div class="filters-grid">
			<Field label="Actor" for="f-actor">
				<select id="f-actor" class="select" bind:value={actor}>
					<option value="">Anyone</option>
					{#each data.accounts as account (account.id)}
						<option value={account.id}>{account.username}</option>
					{/each}
				</select>
			</Field>
			<Field label="Action" for="f-action">
				<select id="f-action" class="select" bind:value={action}>
					<option value="">Any action</option>
					{#each ACTIONS as option (option)}
						<option value={option}>{ACTION_LABELS[option]} · {option}</option>
					{/each}
				</select>
			</Field>
			<Field label="Target" for="f-kind">
				<div class="target">
					<select id="f-kind" class="select" bind:value={targetKind}>
						<option value="">Anything</option>
						{#each TARGET_KINDS as kind (kind)}
							<option value={kind}>{TARGET_KIND_LABELS[kind]}</option>
						{/each}
					</select>
					<input class="input mono" bind:value={targetId} placeholder={targetKind === 'setting' ? 'settings key' : targetKind ? 'id' : 'id'} disabled={!targetKind} aria-label="Target id" />
				</div>
			</Field>
			<Field label="From" for="f-since" error={rangeProblem}>
				<input id="f-since" class="input" type="datetime-local" bind:value={since} />
			</Field>
			<Field label="To" for="f-until">
				<input id="f-until" class="input" type="datetime-local" bind:value={until} />
			</Field>
			<Field label="Per page" for="f-limit">
				<select id="f-limit" class="select" bind:value={limit}>
					{#each LIMITS as option (option)}
						<option value={option}>{option}</option>
					{/each}
				</select>
			</Field>
		</div>
		<div class="filters-foot">
			<Button variant="ghost" onclick={clear} disabled={!filtered && limit === 50}>Clear</Button>
			<Button type="submit" variant="primary" icon="filter" disabled={!!rangeProblem}>Apply filters</Button>
		</div>
	</form>

	{#if entries.length === 0}
		<Empty icon="audit" title={filtered ? 'Nothing matches' : 'Nothing has been recorded yet'} description={filtered ? 'No entry matches these filters.' : 'Settings changes and Discord management actions show up here as they happen.'}>
			{#if filtered}<Button onclick={clear}>Clear filters</Button>{/if}
		</Empty>
	{:else}
		<div class="card">
			<div class="card-header">
				<h2>Entries</h2>
				<span class="faint small">{pluralize(entries.length, 'entry', 'entries')} shown{next ? ', more available' : ''}</span>
			</div>
			<div>
				{#each entries as entry (entry.id)}
					<AuditEntry {entry} />
				{/each}
			</div>
			{#if next || moreError}
				<div class="card-footer">
					{#if moreError}<Alert tone="danger" message={moreError} />{/if}
					{#if next}<Button loading={loadingMore} onclick={loadMore} icon="chevron-down">Load older entries</Button>{/if}
				</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	.filters {
		display: flex;
		flex-direction: column;
	}

	.filters-grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
		gap: 14px;
		padding: 18px 20px;
	}

	.target {
		display: grid;
		grid-template-columns: minmax(132px, 1fr) minmax(0, 1.2fr);
		gap: 6px;
	}

	.filters-foot {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		padding: 12px 20px;
		border-top: 1px solid var(--border);
		background: var(--surface-2);
		border-radius: 0 0 var(--radius) var(--radius);
	}
</style>
