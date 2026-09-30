<script lang="ts">
	import PauseIcon from '@lucide/svelte/icons/pause';
	import PlayIcon from '@lucide/svelte/icons/play';
	import { onMount } from 'svelte';
	import { logs as logsApi, streams } from '$lib/api/endpoints';
	import type { LogLevel, LogLine, LogQuery, Skipped } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status, { type Tone } from '$lib/components/Status.svelte';
	import { dayKey, number } from '$lib/format';
	import DateDivider from '$lib/components/DateDivider.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import { reportError } from '$lib/toast.svelte';

	const LIMIT = 200;
	const MAX_LINES = 5_000;
	const LEVELS: LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

	let lines = $state<LogLine[]>([]);
	let next = $state<number | null>(null);
	let buffered = $state(0);
	let capacity = $state(0);
	let loading = $state(true);
	let loadingMore = $state(false);
	let error = $state<unknown>(null);
	let level = $state<LogLevel | ''>('');
	let target = $state('');
	let q = $state('');
	let paused = $state(false);
	let skipped = $state(0);
	let streamState = $state<'connecting' | 'live' | 'offline'>('connecting');
	let list = $state<HTMLElement | null>(null);
	let stickToBottom = $state(true);

	let source: EventSource | null = null;
	let pendingLines = $state<LogLine[]>([]);

	function filter(): LogQuery {
		return {
			level: level || undefined,
			target: target.trim() || undefined,
			q: q.trim() || undefined
		};
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const page = await logsApi.list({ ...filter(), limit: LIMIT });
			if (current !== requestId) return;
			lines = [...page.lines].reverse();
			next = page.next;
			buffered = page.buffered;
			capacity = page.capacity;
			skipped = 0;
			pendingLines = [];
			stickToBottom = true;
			queueMicrotask(scrollToEnd);
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
		connect();
	}

	async function older() {
		if (next === null || loadingMore) return;
		loadingMore = true;
		try {
			const page = await logsApi.list({ ...filter(), limit: LIMIT, before: next });
			const keepScroll = list ? list.scrollHeight - list.scrollTop : 0;
			lines = [...[...page.lines].reverse(), ...lines];
			next = page.next;
			queueMicrotask(() => {
				if (list) list.scrollTop = list.scrollHeight - keepScroll;
			});
		} catch (err) {
			reportError(err, 'Could not load older lines');
		} finally {
			loadingMore = false;
		}
	}

	function connect() {
		source?.close();
		streamState = 'connecting';
		const es = new EventSource(streams.logs(filter()));
		source = es;
		es.onopen = () => {
			if (source === es) streamState = 'live';
		};
		es.addEventListener('log', (event: MessageEvent<string>) => {
			if (source !== es) return;
			const line = JSON.parse(event.data) as LogLine;
			if (paused) {
				pendingLines.push(line);
				return;
			}
			append([line]);
		});
		es.addEventListener('skipped', (event: MessageEvent<string>) => {
			if (source !== es) return;
			const info = JSON.parse(event.data) as Skipped;
			skipped += info.count;
		});
		es.onerror = () => {
			if (source !== es) return;
			streamState = 'offline';
		};
	}

	function append(fresh: LogLine[]) {
		const merged = [...lines, ...fresh];
		lines = merged.length > MAX_LINES ? merged.slice(merged.length - MAX_LINES) : merged;
		buffered = Math.min(capacity || Infinity, buffered + fresh.length);
		if (stickToBottom) queueMicrotask(scrollToEnd);
	}

	function togglePause() {
		paused = !paused;
		if (!paused && pendingLines.length > 0) {
			append(pendingLines);
			pendingLines = [];
		}
	}

	function scrollToEnd() {
		if (list) list.scrollTop = list.scrollHeight;
	}

	function onScroll() {
		if (!list) return;
		stickToBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
		if (list.scrollTop < 80 && next !== null && !loadingMore) void older();
	}

	onMount(() => {
		void load();
		return () => source?.close();
	});

	const LEVEL_CLASS: Record<LogLevel, string> = {
		trace: 'text-surface-500',
		debug: 'text-secondary-600-400',
		info: 'text-success-600-400',
		warn: 'text-warning-600-400',
		error: 'text-error-600-400'
	};
	const STREAM: Record<typeof streamState, { label: string; tone: Tone }> = {
		connecting: { label: 'Connecting', tone: 'warning' },
		live: { label: 'Live', tone: 'success' },
		offline: { label: 'Reconnecting', tone: 'error' }
	};
</script>

<PageHeader title="Server log" description="The lines the server has kept, with live updates." />

<Card label="Filters">
	<form
		class="grid gap-3 sm:grid-cols-[10rem_16rem_minmax(0,1fr)]"
		onsubmit={(event) => {
			event.preventDefault();
			void load();
		}}
	>
		<label class="label">
			<span class="label-text">Minimum level</span>
			<select class="select" bind:value={level} onchange={() => void load()}>
				<option value="">Every level</option>
				{#each LEVELS as l (l)}<option value={l}>{l} and above</option>{/each}
			</select>
		</label>
		<label class="label">
			<span class="label-text">Target</span>
			<input
				class="input font-mono"
				type="text"
				placeholder="Such as discoclip_engine"
				bind:value={target}
				onchange={() => void load()}
			/>
		</label>
		<div class="label">
			<label class="label-text" for="log-search">Message text</label>
			<SearchInput
				id="log-search"
				bind:value={q}
				placeholder="Message text"
				onsearch={() => void load()}
			/>
		</div>
	</form>
</Card>

<div class="flex flex-wrap items-center justify-between gap-3 text-sm">
	<p class="flex flex-wrap items-center gap-x-4 gap-y-1 text-surface-600-400">
		<span>{number(buffered)} of {number(capacity)} lines retained</span>
		<span>{number(lines.length)} shown</span>
		{#if paused && pendingLines.length > 0}<span>{number(pendingLines.length)} waiting</span>{/if}
		{#if skipped > 0}
			<span class="text-warning-600-400" role="status">
				{number(skipped)} lines skipped while the browser could not keep up
			</span>
		{/if}
	</p>
	<div class="flex items-center gap-3">
		<Status
			label={STREAM[streamState].label}
			tone={STREAM[streamState].tone}
			pulse={streamState !== 'live'}
		/>
		<button type="button" class="btn preset-tonal" onclick={togglePause} aria-pressed={paused}>
			{#if paused}<PlayIcon class="size-4" />Resume{:else}<PauseIcon class="size-4" />Pause{/if}
		</button>
	</div>
</div>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if loading && lines.length === 0}
	<div class="h-96 placeholder animate-pulse" aria-busy="true"></div>
{:else if lines.length === 0}
	<EmptyState
		title="No lines"
		description="Nothing in the buffer matches. New matching lines appear here as they happen."
	/>
{:else}
	<div
		bind:this={list}
		onscroll={onScroll}
		class="h-[65vh] overflow-auto card preset-filled-surface-100-900 p-2 font-mono text-xs"
		role="log"
		aria-live={paused ? 'off' : 'polite'}
	>
		{#if next !== null}
			<div class="flex justify-center py-2">
				<button
					type="button"
					class="btn preset-tonal btn-sm"
					onclick={older}
					disabled={loadingMore}
				>
					{#if loadingMore}<Spinner />{/if}
					Load older
				</button>
			</div>
		{/if}
		<!-- Skeleton disclosures: each line unfolds into its fields. -->
		{#each lines as line, i (line.id)}
			{#if i === 0 || dayKey(line.at) !== dayKey(lines[i - 1].at)}
				<DateDivider at={line.at} />
			{/if}
			<details class="disclosure [--disclosure-size:var(--text-xs)]">
				<summary
					class="grid grid-cols-[auto_auto_auto_minmax(0,1fr)] items-baseline gap-3 whitespace-nowrap"
				>
					<Timestamp at={line.at} mode="clock" class="text-surface-600-400" />
					<span class="w-12 font-semibold uppercase {LEVEL_CLASS[line.level]}">{line.level}</span>
					<span class="max-w-48 truncate text-surface-600-400" title={line.target}
						>{line.target}</span
					>
					<span class="truncate">{line.message}</span>
				</summary>
				<div class="space-y-2 disclosure-content">
					<p class="whitespace-pre-wrap">{line.message}</p>
					{#if Object.keys(line.fields).length > 0}
						<dl class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-0.5">
							{#each Object.entries(line.fields) as [key, value] (key)}
								<dt class="text-surface-600-400">{key}</dt>
								<dd class="break-all">{value}</dd>
							{/each}
						</dl>
					{/if}
				</div>
			</details>
		{/each}
	</div>
{/if}
