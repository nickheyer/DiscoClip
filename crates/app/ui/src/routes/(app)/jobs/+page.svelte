<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import SquareIcon from '@lucide/svelte/icons/square';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import XIcon from '@lucide/svelte/icons/x';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { SvelteURLSearchParams } from 'svelte/reactivity';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { jobs, platforms } from '$lib/api/endpoints';
	import type {
		BulkAction,
		JobOrder,
		JobQuery,
		JobSummary,
		PlatformCoverage,
		StatusKind
	} from '$lib/api/types';
	import Bytes from '$lib/components/Bytes.svelte';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import JobTitle from '$lib/components/JobTitle.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import PlaceLine from '$lib/components/PlaceLine.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { feed } from '$lib/events.svelte';
	import { countText, mediaLabel, number } from '$lib/format';
	import { mergeJobEvent } from '$lib/live';
	import { session } from '$lib/session.svelte';
	import { submitDialog } from '$lib/submit.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const LIMIT = 50;
	const STATUSES: StatusKind[] = ['queued', 'running', 'done', 'failed', 'cancelled'];

	/** The filters, read from the URL so a page can be shared and refreshed. */
	const query = $derived.by((): JobQuery => {
		const p = page.url.searchParams;
		const status = p.get('status');
		const order = p.get('order');
		const offset = Number(p.get('offset') ?? 0);
		return {
			q: p.get('q') ?? undefined,
			status:
				status && STATUSES.includes(status as StatusKind) ? (status as StatusKind) : undefined,
			resolver: p.get('resolver') ?? undefined,
			source: p.get('source') ?? undefined,
			top_level: p.get('top_level') === 'true' ? true : undefined,
			after: p.get('after') ?? undefined,
			before: p.get('before') ?? undefined,
			order: order === 'oldest' ? 'oldest' : undefined,
			offset: Number.isFinite(offset) && offset > 0 ? offset : undefined,
			limit: LIMIT
		};
	});

	let rows = $state<JobSummary[]>([]);
	let total = $state(0);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let selected = $state<string[]>([]);
	let coverage = $state<PlatformCoverage[]>([]);
	let confirmDelete = $state(false);
	let bulkPending = $state<BulkAction | null>(null);

	// Draft filter values, written to the URL on Apply.
	let q = $state('');
	let status = $state<StatusKind | ''>('');
	let resolver = $state('');
	let source = $state('');
	let topLevel = $state(false);
	let after = $state('');
	let before = $state('');
	let order = $state<JobOrder>('newest');

	function syncDraft() {
		q = query.q ?? '';
		status = query.status ?? '';
		resolver = query.resolver ?? '';
		source = query.source ?? '';
		topLevel = query.top_level === true;
		after = toLocalInput(query.after);
		before = toLocalInput(query.before);
		order = query.order ?? 'newest';
	}

	function toLocalInput(timestamp: string | undefined): string {
		if (!timestamp) return '';
		const d = new Date(timestamp);
		if (Number.isNaN(d.getTime())) return '';
		const pad = (n: number) => String(n).padStart(2, '0');
		return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
	}

	function fromLocalInput(text: string): string | undefined {
		if (!text) return undefined;
		const d = new Date(text);
		return Number.isNaN(d.getTime()) ? undefined : d.toISOString();
	}

	async function apply(offset = 0) {
		const params = new SvelteURLSearchParams();
		if (q.trim()) params.set('q', q.trim());
		if (status) params.set('status', status);
		if (resolver.trim()) params.set('resolver', resolver.trim());
		if (source.trim()) params.set('source', source.trim());
		if (topLevel) params.set('top_level', 'true');
		const afterIso = fromLocalInput(after);
		const beforeIso = fromLocalInput(before);
		if (afterIso) params.set('after', afterIso);
		if (beforeIso) params.set('before', beforeIso);
		if (order === 'oldest') params.set('order', 'oldest');
		if (offset > 0) params.set('offset', String(offset));
		const search = params.toString();
		// The path is this page's own route; only the query changes.
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		await goto(search ? `${resolve('/jobs')}?${search}` : resolve('/jobs'), {
			keepFocus: true,
			noScroll: true
		});
	}

	async function clear() {
		q = '';
		status = '';
		resolver = '';
		source = '';
		topLevel = false;
		after = '';
		before = '';
		order = 'newest';
		await apply();
	}

	let requestId = 0;
	async function load() {
		const id = ++requestId;
		loading = true;
		error = null;
		try {
			const result = await jobs.list(query);
			if (id !== requestId) return;
			rows = result.jobs;
			total = result.total;
			selected = selected.filter((key) => rows.some((row) => row.id === key));
		} catch (err) {
			if (id !== requestId) return;
			error = err;
		} finally {
			if (id === requestId) loading = false;
		}
	}

	$effect(() => {
		void query;
		syncDraft();
		void load();
	});

	/** Whether a job belongs on the page with the filters in force. */
	function matches(job: JobSummary): boolean {
		if (query.status && job.status.status !== query.status) return false;
		if (query.resolver && job.resolver !== query.resolver) return false;
		if (query.source && job.source !== query.source) return false;
		if (query.top_level && job.parent !== null) return false;
		if (query.after && job.created_at < query.after) return false;
		if (query.before && job.created_at > query.before) return false;
		if (query.q) {
			const needle = query.q.toLowerCase();
			const hay = [job.title, job.url, job.uploader, job.submitted_by]
				.filter(Boolean)
				.join(' ')
				.toLowerCase();
			if (!hay.includes(needle)) return false;
		}
		return true;
	}

	onMount(() => {
		platforms
			.list()
			.then((list) => (coverage = list))
			.catch(() => (coverage = []));
		return feed.onJob((event) => {
			const before = rows.length;
			const insert = !query.offset && query.order !== 'oldest';
			rows = mergeJobEvent(rows, event, { insert, filter: matches, max: LIMIT });
			if (rows.length > before) total += rows.length - before;
			if (event.kind === 'deleted' && rows.length < before) total = Math.max(0, total - 1);
		});
	});

	async function bulk(action: BulkAction) {
		if (selected.length === 0) return;
		bulkPending = action;
		try {
			const result = await jobs.bulk({ action, ids: selected });
			const verb = { retry: 'retried', cancel: 'cancelled', stop: 'stopped', delete: 'deleted' }[
				action
			];
			const firstError = result.results.find((r) => !r.ok)?.error;
			if (result.failed === 0) {
				notify.success(`${number(result.succeeded)} ${verb}`);
			} else {
				notify.error(
					`${number(result.succeeded)} ${verb}, ${number(result.failed)} failed`,
					firstError ?? undefined
				);
			}
			selected = [];
			await load();
		} catch (err) {
			reportError(err, `Could not ${action} the selected jobs`);
		} finally {
			bulkPending = null;
		}
	}

	const columns: Column<JobSummary>[] = [
		{ key: 'job', label: 'Job', cell: jobCell, class: 'min-w-64' },
		{ key: 'media', label: 'Media', cell: mediaCell },
		{ key: 'status', label: 'Status', cell: statusCell },
		{ key: 'origin', label: 'From', cell: originCell },
		{ key: 'size', label: 'Size', align: 'right', cell: sizeCell },
		{ key: 'age', label: 'Submitted', cell: ageCell }
	];

	const canManage = $derived(session.can('manage_jobs'));
	/** Whether a selected job is capturing a live stream, which Stop ends while keeping the recording. */
	const capturingSelected = $derived(
		rows.some(
			(job) =>
				selected.includes(job.id) &&
				job.recording &&
				job.status.status === 'running' &&
				job.status.stage === 'download'
		)
	);
</script>

{#snippet jobCell(job: JobSummary)}
	<JobTitle {job} />
{/snippet}
{#snippet mediaCell(job: JobSummary)}
	<span class="inline-flex items-center gap-1.5" title={mediaLabel(job.media)}>
		<MediaKindIcon kind={job.media} />
		{#if job.live}<span class="text-error-600-400">Live</span>{/if}
		{#if job.children > 0}
			<span class="text-surface-600-400">{number(job.children)} entries</span>
		{/if}
	</span>
{/snippet}
{#snippet statusCell(job: JobSummary)}
	<Status job={job.status} />
{/snippet}
{#snippet originCell(job: JobSummary)}
	<PlaceLine
		origin={job.origin}
		place={job.place}
		destination={job.destination}
		submittedBy={job.submitted_by}
		compact
		class="max-w-72"
	/>
{/snippet}
{#snippet sizeCell(job: JobSummary)}
	<Bytes value={job.output_bytes} />
{/snippet}
{#snippet ageCell(job: JobSummary)}
	<Timestamp at={job.created_at} class="whitespace-nowrap" />
{/snippet}

<PageHeader title="Jobs" description="Every link the server has been asked to fetch.">
	{#snippet actions()}
		{#if canManage}
			<button
				type="button"
				class="btn preset-filled-primary-500"
				onclick={() => (submitDialog.open = true)}
			>
				<PlusIcon class="size-4" />
				Submit a link
			</button>
		{/if}
	{/snippet}
</PageHeader>

<Card label="Filters">
	<form
		class="grid gap-4 sm:grid-cols-2 xl:grid-cols-4"
		onsubmit={(event) => {
			event.preventDefault();
			void apply();
		}}
	>
		<Field label="Search jobs" for="job-search" class="sm:col-span-2">
			<SearchInput
				id="job-search"
				bind:value={q}
				placeholder="Title, link, or uploader"
				onsearch={() => void apply()}
			/>
		</Field>
		<Field label="Status" for="job-status">
			<select id="job-status" class="select" bind:value={status}>
				<option value="">Any status</option>
				{#each STATUSES as kind (kind)}
					<option value={kind}>{kind[0].toUpperCase() + kind.slice(1)}</option>
				{/each}
			</select>
		</Field>
		<Field label="Sort by" for="job-order">
			<select id="job-order" class="select" bind:value={order}>
				<option value="newest">Newest first</option>
				<option value="oldest">Oldest first</option>
			</select>
		</Field>
		<Field label="Resolver" for="job-resolver">
			<input
				id="job-resolver"
				class="input"
				list="resolvers"
				placeholder="Any resolver"
				bind:value={resolver}
			/>
		</Field>
		<datalist id="resolvers">
			{#each coverage as platform (platform.id)}
				<option value={platform.id}>{platform.name}</option>
			{/each}
		</datalist>
		<Field label="Source" for="job-source">
			<input id="job-source" class="input" placeholder="For example, Discord" bind:value={source} />
		</Field>
		<Field label="Submitted after" for="job-after">
			<input id="job-after" class="input" type="datetime-local" bind:value={after} />
		</Field>
		<Field label="Submitted before" for="job-before">
			<input id="job-before" class="input" type="datetime-local" bind:value={before} />
		</Field>
		<hr class="hr sm:col-span-2 xl:col-span-4" />
		<div class="flex flex-wrap items-center justify-between gap-4 sm:col-span-2 xl:col-span-4">
			<Switch checked={topLevel} onCheckedChange={(details) => (topLevel = details.checked)}>
				<Switch.Control><Switch.Thumb /></Switch.Control>
				<Switch.Label>Hide playlist entries</Switch.Label>
				<Switch.HiddenInput />
			</Switch>
			<div class="flex gap-2">
				<button type="button" class="btn preset-tonal" onclick={clear}>Clear filters</button>
				<button type="submit" class="btn preset-filled">Apply filters</button>
			</div>
		</div>
	</form>
</Card>

{#if canManage && selected.length > 0}
	<div
		class="flex flex-wrap items-center gap-2 card preset-tonal-primary p-3"
		role="toolbar"
		aria-label="Selected jobs"
	>
		<span class="font-medium">{number(selected.length)} selected</span>
		<span class="flex-1"></span>
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={() => bulk('retry')}
			disabled={bulkPending !== null}
		>
			{#if bulkPending === 'retry'}<Spinner />{:else}<RotateCcwIcon class="size-4" />{/if}
			Retry
		</button>
		{#if capturingSelected}
			<button
				type="button"
				class="btn preset-tonal btn-sm"
				onclick={() => bulk('stop')}
				disabled={bulkPending !== null}
			>
				{#if bulkPending === 'stop'}<Spinner />{:else}<SquareIcon class="size-4" />{/if}
				Stop
			</button>
		{/if}
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={() => bulk('cancel')}
			disabled={bulkPending !== null}
		>
			{#if bulkPending === 'cancel'}<Spinner />{:else}<XIcon class="size-4" />{/if}
			Cancel
		</button>
		<button
			type="button"
			class="btn preset-tonal-error btn-sm"
			onclick={() => (confirmDelete = true)}
			disabled={bulkPending !== null}
		>
			{#if bulkPending === 'delete'}<Spinner />{:else}<Trash2Icon class="size-4" />{/if}
			Delete
		</button>
		<button type="button" class="btn btn-sm hover:preset-tonal" onclick={() => (selected = [])}>
			Clear selection
		</button>
	</div>
{/if}

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<DataTable
		{rows}
		{columns}
		rowKey={(job) => job.id}
		{loading}
		selectable={canManage}
		bind:selected
		rowHref={(job) => resolve('/(app)/jobs/[id]', { id: job.id })}
		rowLabel="View"
	>
		{#snippet empty()}
			No jobs match these filters.
		{/snippet}
	</DataTable>
	<Pager
		{total}
		limit={LIMIT}
		offset={query.offset ?? 0}
		onchange={(offset) => void apply(offset)}
	/>
{/if}

<Confirm
	bind:open={confirmDelete}
	title="Delete {countText(selected.length, 'job')}?"
	message="Their records and cached files go away. Running jobs are skipped."
	confirmLabel="Delete"
	danger
	onconfirm={() => bulk('delete')}
/>
