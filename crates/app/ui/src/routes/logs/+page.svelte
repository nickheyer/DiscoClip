<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import type { PageData } from './$types';
	import { logs, messageOf } from '$lib/api';
	import { LOG_LEVELS } from '$lib/api';
	import type { LogLevel, LogLine, Skipped } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import { formatDateTime, formatNumber } from '$lib/format';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const LEVEL_LABELS: Record<LogLevel, string> = {
		trace: 'Trace',
		debug: 'Debug',
		info: 'Info',
		warn: 'Warning',
		error: 'Error'
	};

	function levelTone(level: LogLevel): 'neutral' | 'info' | 'warn' | 'danger' {
		switch (level) {
			case 'error':
				return 'danger';
			case 'warn':
				return 'warn';
			case 'info':
				return 'info';
			default:
				return 'neutral';
		}
	}

	// Filters: applied by loading again, and to the lines that arrive live.
	let level = $state<LogLevel | ''>('');
	let target = $state('');
	let q = $state('');

	// The page's load seeds the lines once; from then on the page keeps them itself, oldest
	// first, so the newest sit at the bottom like a terminal.
	const first = untrack(() => data.first);
	let lines = $state<LogLine[]>([...first.lines].reverse());
	let next = $state<number | null>(first.next);
	let buffered = $state(first.buffered);
	let capacity = $state(first.capacity);
	let skipped = $state(0);
	let loading = $state(false);
	let loadingOlder = $state(false);
	let error = $state<string | null>(null);

	// Following
	let following = $state(true);
	let feed = $state<'connecting' | 'live' | 'reconnecting' | 'off'>('off');
	let source: EventSource | null = null;
	let list: HTMLDivElement | undefined = $state();
	let atBottom = $state(true);

	const query = $derived({
		level: level || undefined,
		target: target.trim() || undefined,
		q: q.trim() || undefined
	});

	async function reload() {
		loading = true;
		error = null;
		try {
			const page = await logs.list({ ...query, limit: 200 });
			lines = [...page.lines].reverse();
			next = page.next;
			buffered = page.buffered;
			capacity = page.capacity;
			skipped = 0;
			await scrollToEnd();
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			loading = false;
		}
	}

	async function older() {
		if (next === null) return;
		loadingOlder = true;
		try {
			const page = await logs.list({ ...query, limit: 200, before: next });
			const keep = list ? list.scrollHeight - list.scrollTop : 0;
			lines = [...[...page.lines].reverse(), ...lines];
			next = page.next;
			await tick();
			if (list) list.scrollTop = list.scrollHeight - keep;
		} catch (cause) {
			toast.error(`Could not load older lines: ${messageOf(cause)}`);
		} finally {
			loadingOlder = false;
		}
	}

	function follow() {
		unfollow();
		feed = 'connecting';
		const es = new EventSource(logs.eventsUrl(query), { withCredentials: true });
		es.onopen = () => (feed = 'live');
		es.onerror = () => (feed = es.readyState === EventSource.CLOSED ? 'off' : 'reconnecting');
		es.addEventListener('log', (event) => {
			const line = JSON.parse((event as MessageEvent).data) as LogLine;
			lines = [...lines, line].slice(-2000);
			buffered = Math.min(capacity, buffered + 1);
			if (atBottom) void scrollToEnd();
		});
		es.addEventListener('skipped', (event) => {
			skipped += (JSON.parse((event as MessageEvent).data) as Skipped).count;
		});
		source = es;
	}

	function unfollow() {
		source?.close();
		source = null;
		feed = 'off';
	}

	async function scrollToEnd() {
		await tick();
		if (list) list.scrollTop = list.scrollHeight;
	}

	function onScroll() {
		if (!list) return;
		atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 24;
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function onFilterInput() {
		clearTimeout(debounce);
		debounce = setTimeout(() => void applyFilters(), 300);
	}

	async function applyFilters() {
		await reload();
		if (following) follow();
	}

	$effect(() => {
		if (following) follow();
		else unfollow();
		return unfollow;
	});

	onMount(() => {
		void scrollToEnd();
		return () => {
			clearTimeout(debounce);
			unfollow();
		};
	});

	const feedLabel = $derived(
		!following ? 'Paused' : feed === 'live' ? 'Following' : feed === 'connecting' ? 'Connecting' : feed === 'reconnecting' ? 'Reconnecting' : 'Not following'
	);
</script>

<svelte:head>
	<title>Log · DiscoClip</title>
</svelte:head>

<PageHeader title="Log" description="What the server writes to its log, as it is written. The filter set in settings decides what is written at all.">
	{#snippet meta()}
		<Badge tone={following && feed === 'live' ? 'ok' : following ? 'warn' : 'neutral'} size="sm" dot pulse={following && feed !== 'live'}>{feedLabel}</Badge>
		<span class="faint small">{formatNumber(buffered)} of {formatNumber(capacity)} lines kept on the server</span>
	{/snippet}
	{#snippet actions()}
		<Button icon={following ? 'stop' : 'play'} onclick={() => (following = !following)}>{following ? 'Pause' : 'Follow'}</Button>
		<Button icon="refresh" loading={loading} onclick={reload}>Reload</Button>
		{#if session.can('manage_settings')}
			<Button href="/settings#log" icon="settings">Log filter</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack">
	<div class="filters">
		<select class="select level" bind:value={level} onchange={applyFilters} aria-label="Lowest level">
			<option value="">Every level</option>
			{#each LOG_LEVELS as l (l)}
				<option value={l}>{LEVEL_LABELS[l]} and above</option>
			{/each}
		</select>
		<input class="input" type="search" placeholder="Target, such as discoclip_engine" bind:value={target} oninput={onFilterInput} aria-label="Target" />
		<input class="input" type="search" placeholder="Search messages and fields" bind:value={q} oninput={onFilterInput} aria-label="Search" />
	</div>

	{#if error}
		<Alert tone="danger" message={error} onclose={() => (error = null)} />
	{/if}
	{#if skipped > 0}
		<Alert tone="warn" message={`${formatNumber(skipped)} lines were written faster than this page could read them and are not shown; reload to see what the server kept.`} />
	{/if}

	<div class="viewer card">
		<div class="viewer-head">
			{#if next !== null}
				<Button size="sm" variant="ghost" icon="chevron-left" loading={loadingOlder} onclick={older}>Older lines</Button>
			{:else}
				<span class="faint small">{lines.length ? 'The oldest lines the server keeps.' : ''}</span>
			{/if}
			<span class="faint small">{formatNumber(lines.length)} shown</span>
		</div>
		<div class="lines" bind:this={list} onscroll={onScroll} role="log" aria-live="off">
			{#if lines.length === 0}
				<Empty compact icon="file-text" title="Nothing to show" description={level || target || q ? 'No kept line matches the filter.' : 'The server has written nothing since it started.'} />
			{:else}
				{#each lines as line (line.id)}
					<div class={['line', `line-${line.level}`]}>
						<span class="when" title={line.at}>{formatDateTime(line.at, true)}</span>
						<span class="lvl"><Badge tone={levelTone(line.level)} size="sm">{line.level.toUpperCase()}</Badge></span>
						<span class="target" title={line.target}>{line.target}</span>
						<span class="msg">
							{line.message}
							{#each Object.entries(line.fields) as [key, value] (key)}
								<span class="field"><span class="key">{key}</span>=<span class="val">{value}</span></span>
							{/each}
						</span>
					</div>
				{/each}
			{/if}
		</div>
		{#if !atBottom && lines.length}
			<button type="button" class="jump" onclick={scrollToEnd}>Jump to the newest</button>
		{/if}
	</div>
</div>

<style>
	.filters {
		display: grid;
		grid-template-columns: 180px minmax(0, 1fr) minmax(0, 1fr);
		gap: 8px;
	}

	.viewer {
		position: relative;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}

	.viewer-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 6px 12px;
		border-bottom: 1px solid var(--border);
		background: var(--surface-2);
	}

	.lines {
		height: min(70vh, 720px);
		overflow: auto;
		padding: 6px 0;
		font-family: var(--font-mono);
		font-size: 12.5px;
		line-height: 1.5;
	}

	.line {
		display: grid;
		grid-template-columns: 150px 64px 200px minmax(0, 1fr);
		gap: 10px;
		padding: 2px 12px;
		align-items: baseline;
	}

	.line:hover {
		background: color-mix(in srgb, var(--surface-2) 70%, transparent);
	}

	.line-error {
		background: color-mix(in srgb, var(--danger-soft) 55%, transparent);
	}

	.line-warn {
		background: color-mix(in srgb, var(--warn-soft) 45%, transparent);
	}

	.when {
		color: var(--text-3);
		white-space: nowrap;
	}

	.target {
		color: var(--text-3);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.msg {
		overflow-wrap: anywhere;
	}

	.field {
		margin-left: 10px;
		color: var(--text-2);
	}

	.key {
		color: var(--accent-text);
	}

	.jump {
		position: absolute;
		right: 16px;
		bottom: 14px;
		padding: 6px 12px;
		border: 1px solid var(--border-strong);
		border-radius: 999px;
		background: var(--surface);
		box-shadow: var(--shadow);
		font-size: 12.5px;
		cursor: pointer;
	}

	@media (max-width: 800px) {
		.filters {
			grid-template-columns: 1fr;
		}

		.line {
			grid-template-columns: 1fr;
			gap: 2px;
			padding: 6px 12px;
			border-bottom: 1px solid var(--border);
		}
	}
</style>
