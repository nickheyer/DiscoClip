<script lang="ts">
	import PauseIcon from '@lucide/svelte/icons/pause';
	import PlayIcon from '@lucide/svelte/icons/play';
	import { onMount } from 'svelte';
	import { logs as logsApi, streams } from '$lib/api/endpoints';
	import type { LogLevel, LogLine, LogQuery, Skipped } from '$lib/api/types';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { absolute, number } from '$lib/format';
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
		trace: 'text-surface-400',
		debug: 'text-secondary-800-200',
		info: 'text-success-800-200',
		warn: 'text-warning-800-200',
		error: 'text-error-700-300'
	};
	const STREAM = {
		connecting: { label: 'Connecting', dot: 'bg-warning-500' },
		live: { label: 'Live', dot: 'bg-success-500' },
		offline: { label: 'Reconnecting', dot: 'bg-error-500' }
	} as const;
</script>

<PageHeader title="Server log" />

<form
	class="flex flex-wrap gap-3 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
	onsubmit={(event) => {
		event.preventDefault();
		void load();
	}}
>
	<select
		class="select w-full sm:w-40"
		bind:value={level}
		onchange={() => void load()}
		aria-label="Minimum level"
	>
		<option value="">Every level</option>
		{#each LEVELS as l (l)}<option value={l}>{l} and above</option>{/each}
	</select>
	<input
		class="input w-full font-mono text-sm sm:w-64"
		type="text"
		placeholder="Target, such as discoclip_engine"
		aria-label="Target"
		bind:value={target}
		onchange={() => void load()}
	/>
	<SearchInput
		bind:value={q}
		placeholder="Message text"
		onsearch={() => void load()}
		class="w-full sm:min-w-56 sm:flex-1"
	/>
</form>

<Toolbar description="The lines the server has kept, with live updates.">
	<p class="mr-auto flex flex-wrap gap-x-4 text-sm text-surface-600-400">
		<span>{number(buffered)} of {number(capacity)} lines retained</span>
		<span>{number(lines.length)} shown</span>
		{#if paused && pendingLines.length > 0}<span>{number(pendingLines.length)} waiting</span>{/if}
		{#if skipped > 0}
			<span class="text-warning-800-200" role="status"
				>{number(skipped)} lines skipped while the browser could not keep up</span
			>
		{/if}
	</p>
	<span class="inline-flex items-center gap-2 text-sm text-surface-600-400">
		<span class="size-2 rounded-full {STREAM[streamState].dot}" aria-hidden="true"></span>
		{STREAM[streamState].label}
	</span>
	<button type="button" class="btn preset-tonal" onclick={togglePause} aria-pressed={paused}>
		{#if paused}<PlayIcon class="size-4" />Resume{:else}<PauseIcon class="size-4" />Pause{/if}
	</button>
</Toolbar>

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
		class="h-[65vh] overflow-auto rounded-container border border-surface-200-800 bg-surface-950 font-mono text-sm text-surface-100"
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
		{#each lines as line (line.id)}
			<details class="group border-b border-surface-800/60 px-4 py-2 open:bg-surface-900">
				<summary
					class="grid cursor-pointer list-none grid-cols-[auto_auto_auto_1fr] items-baseline gap-3 whitespace-nowrap"
				>
					<time datetime={line.at} class="text-surface-400">{absolute(line.at)}</time>
					<span class="w-12 font-semibold uppercase {LEVEL_CLASS[line.level]}">{line.level}</span>
					<span class="max-w-48 truncate text-surface-400" title={line.target}>{line.target}</span>
					<span class="truncate whitespace-pre-wrap group-open:whitespace-normal"
						>{line.message}</span
					>
				</summary>
				{#if Object.keys(line.fields).length > 0}
					<dl class="mt-1 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 pl-6 text-surface-300">
						{#each Object.entries(line.fields) as [key, value] (key)}
							<dt class="text-surface-400">{key}</dt>
							<dd class="break-all">{value}</dd>
						{/each}
					</dl>
				{/if}
			</details>
		{/each}
	</div>
{/if}
