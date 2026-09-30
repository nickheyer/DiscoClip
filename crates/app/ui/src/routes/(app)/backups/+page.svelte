<script lang="ts">
	import DownloadIcon from '@lucide/svelte/icons/download';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { goto, preloadCode } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { backups as backupsApi, retention as retentionApi, settings } from '$lib/api/endpoints';
	import type { BackupEntry, BackupsView, RetentionView } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Bytes from '$lib/components/Bytes.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { absolute, bytes, countText, durationText, number } from '$lib/format';
	import { notify, reportError } from '$lib/toast.svelte';

	let view = $state<BackupsView | null>(null);
	let retention = $state<RetentionView | null>(null);
	let error = $state<unknown>(null);
	let retentionError = $state<unknown>(null);
	let running = $state(false);
	let sweeping = $state(false);
	let deleting = $state<BackupEntry | null>(null);
	let deleteOpen = $state(false);
	let restoring = $state<BackupEntry | null>(null);
	let restoreOpen = $state(false);
	let restoringNow = $state(false);
	let scheduleOpen = $state(false);
	let saving = $state(false);
	let enabled = $state(true);
	let interval = $state(86400);
	let keep = $state<number | undefined>(7);
	let scheduleError = $state('');

	/** The frequencies offered, in seconds. */
	const intervals = [3600, 21600, 43200, 86400, 604800];
	const lastSweep = $derived(retention?.status.last);
	const totalBytes = $derived(view?.backups.reduce((sum, backup) => sum + backup.bytes, 0) ?? 0);
	const busy = $derived(running || saving || restoringNow);

	async function loadBackups() {
		error = null;
		try {
			view = await backupsApi.list();
		} catch (err) {
			error = err;
		}
	}

	async function loadRetention() {
		retentionError = null;
		try {
			retention = await retentionApi.get();
		} catch (err) {
			retentionError = err;
		}
	}

	onMount(() => {
		void loadBackups();
		void loadRetention();
	});

	async function run() {
		running = true;
		try {
			await backupsApi.run();
			notify.success('Backup created');
			await loadBackups();
		} catch (err) {
			reportError(err, 'Could not create backup');
		} finally {
			running = false;
		}
	}

	async function sweep() {
		sweeping = true;
		try {
			const report = await retentionApi.sweep();
			const removed = report.jobs_removed + report.failed_removed;
			if (report.error) notify.error('Cleanup incomplete', report.error);
			else
				notify.success(
					'Cleanup complete',
					`${countText(removed, 'job')} removed · ${bytes(report.bytes_freed)} freed`
				);
			await loadRetention();
		} catch (err) {
			reportError(err, 'Could not run cleanup');
		} finally {
			sweeping = false;
		}
	}

	async function remove() {
		if (!deleting) return;
		await backupsApi.remove(deleting.name);
		notify.success('Backup deleted');
		await loadBackups();
	}

	async function restore() {
		if (!restoring) return;
		restoringNow = true;
		try {
			await preloadCode(resolve('/restore'));
			const progress = await backupsApi.restore(restoring.name);
			// The route is resolved before adding the restore receipt.
			// eslint-disable-next-line svelte/no-navigation-without-resolve
			await goto(`${resolve('/restore')}?id=${encodeURIComponent(progress.id)}`);
		} finally {
			restoringNow = false;
		}
	}

	function editSchedule() {
		if (!view) return;
		enabled = view.enabled;
		interval = view.interval_secs;
		keep = view.keep;
		scheduleError = '';
		scheduleOpen = true;
	}

	async function saveSchedule(event: SubmitEvent) {
		event.preventDefault();
		if (!keep || !Number.isInteger(keep) || keep < 1) {
			scheduleError = 'Keep at least one backup.';
			return;
		}
		saving = true;
		scheduleError = '';
		try {
			await settings.change({
				set: { 'backup.enabled': enabled, 'backup.interval_secs': interval, 'backup.keep': keep }
			});
			scheduleOpen = false;
			notify.success('Schedule updated');
			await loadBackups();
		} catch (err) {
			scheduleError = err instanceof Error ? err.message : String(err);
		} finally {
			saving = false;
		}
	}

	const columns: Column<BackupEntry>[] = [
		{ key: 'created', label: 'Created', cell: createdCell },
		{ key: 'size', label: 'Size', align: 'right', value: (backup) => bytes(backup.bytes) },
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: actionsCell, align: 'right' }
	];
</script>

{#snippet createdCell(backup: BackupEntry)}
	<span class="flex flex-wrap items-center gap-2 font-medium" title={backup.name}>
		<Timestamp at={backup.at} class="whitespace-nowrap" />
		{#if view?.backups[0]?.name === backup.name}
			<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">Latest</span>
		{/if}
	</span>
{/snippet}
{#snippet actionsCell(backup: BackupEntry)}
	<span class="flex items-center justify-end gap-1">
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			disabled={busy}
			aria-label="Restore backup from {absolute(backup.at)}"
			onclick={() => {
				restoring = backup;
				restoreOpen = true;
			}}
		>
			<RotateCcwIcon class="size-4" />
			Restore
		</button>
		<a
			class="btn-icon btn-icon-sm hover:preset-tonal"
			href={backupsApi.downloadUrl(backup.name)}
			download={backup.name}
			title="Download backup"
			aria-label="Download {backup.name}"
		>
			<DownloadIcon />
		</a>
		<button
			type="button"
			class="btn-icon btn-icon-sm hover:preset-tonal-error"
			disabled={busy}
			title="Delete backup"
			aria-label="Delete {backup.name}"
			onclick={() => {
				deleting = backup;
				deleteOpen = true;
			}}
		>
			<Trash2Icon />
		</button>
	</span>
{/snippet}

<PageHeader title="Backups" description="Settings, accounts, and job history.">
	{#snippet actions()}
		<button
			type="button"
			class="btn preset-filled-primary-500"
			onclick={run}
			disabled={busy || !view}
		>
			{#if running}<Spinner />{:else}<PlusIcon class="size-4" />{/if}
			{running ? 'Creating backup…' : 'Create backup'}
		</button>
	{/snippet}
</PageHeader>

{#if error}
	<ErrorState {error} onretry={loadBackups} />
{:else if !view}
	<div class="space-y-4" aria-busy="true" aria-label="Loading backups">
		<div class="h-32 placeholder animate-pulse"></div>
		<div class="h-48 placeholder animate-pulse"></div>
	</div>
{:else}
	<Card title="Automatic backups">
		{#snippet actions()}
			<button type="button" class="btn preset-tonal btn-sm" onclick={editSchedule} disabled={busy}>
				Edit schedule
			</button>
		{/snippet}
		<div class="space-y-3">
			<KeyValue class="sm:grid-cols-[auto_minmax(0,1fr)_auto_minmax(0,1fr)_auto_minmax(0,1fr)]">
				<KeyValueRow label="State">
					<Status enabled={view.enabled} />
				</KeyValueRow>
				<KeyValueRow label="Frequency"><Duration value={view.interval_secs} /></KeyValueRow>
				<KeyValueRow label="Keep" value="Latest {number(view.keep)}" />
			</KeyValue>
			{#if view.status.last_error}
				<p class="card preset-tonal-error p-3 text-sm" role="alert">
					The last backup failed. {view.status.last_error}
				</p>
			{/if}
		</div>
	</Card>

	<Card
		title="Saved backups"
		count={number(view.backups.length)}
		description="Media files are not included in database backups."
		flush
	>
		{#snippet actions()}
			{#if view && view.backups.length > 0}
				<span class="text-sm text-surface-600-400">{bytes(totalBytes)} total</span>
			{/if}
		{/snippet}
		{#if view.backups.length === 0}
			<EmptyState
				contained
				title="No backups yet"
				description="Create your first backup to have a point to return to."
			>
				<button type="button" class="btn preset-tonal" onclick={run} disabled={busy}>
					{#if running}<Spinner />{/if}
					Create first backup
				</button>
			</EmptyState>
		{:else}
			<DataTable rows={view.backups} {columns} rowKey={(backup) => backup.name} flush class="p-2" />
		{/if}
	</Card>
{/if}

<Card title="Data cleanup" description="Remove old jobs and free up cached media.">
	{#snippet actions()}
		<button
			type="button"
			class="btn preset-tonal btn-sm"
			onclick={sweep}
			disabled={sweeping || busy || !retention}
		>
			{#if sweeping}<Spinner />{/if}
			{sweeping ? 'Cleaning up…' : 'Run cleanup'}
		</button>
	{/snippet}
	{#if retentionError}
		<ErrorState error={retentionError} onretry={loadRetention} />
	{:else if retention}
		<div class="space-y-3">
			<KeyValue class="sm:grid-cols-[auto_minmax(0,1fr)_auto_minmax(0,1fr)]">
				<KeyValueRow
					label="Completed jobs"
					value={retention.config.jobs_days
						? `Keep for ${number(retention.config.jobs_days)} days`
						: 'Keep forever'}
				/>
				<KeyValueRow
					label="Failed jobs"
					value={retention.config.failed_jobs_days
						? `Keep for ${number(retention.config.failed_jobs_days)} days`
						: 'Keep forever'}
				/>
				<KeyValueRow
					label="Cache limit"
					value={retention.config.cache_max_bytes
						? bytes(retention.config.cache_max_bytes)
						: 'No limit'}
				/>
				<KeyValueRow label="Automatic cleanup">
					<Duration value={retention.config.sweep_interval_secs} />
				</KeyValueRow>
			</KeyValue>
			<p class="text-sm text-surface-600-400">
				{#if lastSweep}
					Last run <Timestamp at={lastSweep.at} /> · {bytes(lastSweep.bytes_freed)} freed
				{:else}
					No cleanup runs yet
				{/if}
			</p>
			{#if lastSweep?.error}
				<p class="card preset-tonal-error p-3 text-sm" role="alert">{lastSweep.error}</p>
			{/if}
		</div>
	{:else}
		<div class="space-y-3" aria-busy="true" aria-label="Loading cleanup settings">
			<div class="h-4 placeholder animate-pulse"></div>
			<div class="h-4 placeholder w-3/4 animate-pulse"></div>
		</div>
	{/if}
</Card>

<Confirm
	bind:open={deleteOpen}
	title="Delete this backup?"
	message="This permanently deletes the saved copy. Your current database is unaffected."
	confirmLabel="Delete backup"
	danger
	onconfirm={remove}
>
	{#if deleting}
		<p class="card preset-tonal p-3 text-sm font-medium">
			<Timestamp at={deleting.at} />
			<span class="text-surface-600-400">· <Bytes value={deleting.bytes} /></span>
		</p>
	{/if}
</Confirm>
<Confirm
	bind:open={restoreOpen}
	title="Restore this backup?"
	message="This replaces your settings, accounts, and job history with the saved copy."
	confirmLabel="Restore backup"
	onconfirm={restore}
>
	{#if restoring}
		<p class="card preset-tonal p-3 text-sm font-medium">
			<Timestamp at={restoring.at} />
			<span class="text-surface-600-400">· <Bytes value={restoring.bytes} /></span>
		</p>
	{/if}
	<div class="space-y-2 text-sm text-surface-600-400">
		<p>
			A backup of your current database is saved first. DiscoClip will briefly go offline, then
			restart automatically.
		</p>
		<p>
			Your server connection and backup location stay the same. You will sign in with an account
			from the restored backup.
		</p>
	</div>
</Confirm>

<Modal bind:open={scheduleOpen} title="Backup schedule" size="sm" busy={saving}>
	<form id="backup-schedule" class="space-y-4" onsubmit={saveSchedule}>
		<Switch
			checked={enabled}
			onCheckedChange={(details) => (enabled = details.checked)}
			disabled={saving}
			class="flex w-full justify-between"
		>
			<Switch.Label>Automatic backups</Switch.Label>
			<Switch.Control><Switch.Thumb /></Switch.Control>
			<Switch.HiddenInput />
		</Switch>
		<Field label="Frequency" for="backup-interval">
			<select
				id="backup-interval"
				class="select"
				bind:value={interval}
				disabled={!enabled || saving}
			>
				{#if !intervals.includes(interval)}
					<option value={interval}>{durationText(interval)}</option>
				{/if}
				{#each intervals as option (option)}
					<option value={option}>{durationText(option)}</option>
				{/each}
			</select>
		</Field>
		<Field
			label="Backups to keep"
			for="backup-keep"
			help="Older copies are removed after a new backup is created."
			error={scheduleError || null}
		>
			<input
				id="backup-keep"
				class="input"
				type="number"
				min="1"
				max="4294967295"
				step="1"
				required
				bind:value={keep}
				disabled={saving}
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (scheduleOpen = false)}
			disabled={saving}>Cancel</button
		>
		<button
			type="submit"
			form="backup-schedule"
			class="btn preset-filled-primary-500"
			disabled={saving}
		>
			{#if saving}<Spinner />{/if}
			Save schedule
		</button>
	{/snippet}
</Modal>
