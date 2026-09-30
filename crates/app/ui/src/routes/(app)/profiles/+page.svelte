<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { profiles as profilesApi } from '$lib/api/endpoints';
	import type { Profile, Uuid } from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Bytes from '$lib/components/Bytes.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	let profiles = $state<Profile[]>([]);
	let defaultId = $state<Uuid | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let settingDefault = $state<Uuid | null>(null);
	let deleting = $state<Profile | null>(null);
	let deleteOpen = $state(false);

	const canEdit = $derived(session.can('manage_settings'));

	async function load() {
		loading = true;
		error = null;
		try {
			const [list, effective] = await Promise.all([profilesApi.list(), profilesApi.effective()]);
			profiles = list;
			defaultId = effective.applied.find((a) => a.scope.kind === 'global')?.profile_id ?? null;
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	async function setDefault(profile: Profile) {
		settingDefault = profile.id;
		try {
			await profilesApi.assign('global', profile.id);
			defaultId = profile.id;
			notify.success('Global default set', profile.name);
		} catch (err) {
			reportError(err, 'Could not set the default');
		} finally {
			settingDefault = null;
		}
	}

	function askDelete(profile: Profile) {
		deleting = profile;
		deleteOpen = true;
	}

	async function remove() {
		if (!deleting) return;
		await profilesApi.remove(deleting.id);
		notify.success('Profile deleted', deleting.name);
		deleting = null;
		await load();
	}

	/** Whether a profile names any limit of its own. */
	function limited(profile: Profile): boolean {
		const l = profile.limits;
		return Boolean(
			l?.max_source_bytes ||
			(l?.max_duration_secs !== null && l?.max_duration_secs !== undefined) ||
			l?.max_height ||
			l?.max_capture_secs
		);
	}

	function platforms(profile: Profile): string {
		const p = profile.platforms;
		const overrides = Object.keys(p?.overrides ?? {}).length;
		const base =
			p?.presets && p.presets.length > 0
				? `${p.presets.join(', ')}`
				: { inherit: 'Inherit', enabled: 'All on', disabled: 'All off' }[p?.default ?? 'inherit'];
		return overrides > 0 ? `${base} · ${number(overrides)} exceptions` : base;
	}

	const columns: Column<Profile>[] = [
		{ key: 'name', label: 'Profile', cell: nameCell, sortable: true, value: (p) => p.name },
		{ key: 'limits', label: 'Limits', cell: limitsCell },
		{ key: 'platforms', label: 'Platforms', value: platforms, class: 'max-w-64 truncate' },
		{
			key: 'updated',
			label: 'Updated',
			cell: updatedCell,
			sortable: true,
			value: (p) => p.updated_at
		},
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: actionsCell, align: 'right' }
	];
</script>

{#snippet limitsCell(profile: Profile)}
	{@const l = profile.limits}
	{#if limited(profile)}
		<span class="flex flex-wrap items-center gap-x-2 gap-y-1">
			{#if l?.max_source_bytes}<Bytes value={l.max_source_bytes} />{/if}
			{#if l?.max_duration_secs === 0}
				<Status label="No live" tone="warning" />
			{:else if l?.max_duration_secs}
				<Duration value={l.max_duration_secs} />
			{/if}
			{#if l?.max_height}<span>{l.max_height} px</span>{/if}
			{#if l?.max_capture_secs}<span>capture <Duration value={l.max_capture_secs} /></span>{/if}
		</span>
	{:else}
		{profile.id === defaultId ? 'Server limits' : 'Inherited'}
	{/if}
{/snippet}
{#snippet nameCell(profile: Profile)}
	<div class="min-w-0">
		<p class="flex flex-wrap items-center gap-2 font-medium">
			{profile.name}
			{#if profile.id === defaultId}
				<Status label="Default" tone="primary" />
			{/if}
			{#if profile.builtin}
				<Status label="Built in" tone="surface" />
			{/if}
		</p>
		{#if profile.description}
			<p class="truncate text-sm text-surface-600-400">{profile.description}</p>
		{/if}
	</div>
{/snippet}
{#snippet updatedCell(profile: Profile)}
	<Timestamp at={profile.updated_at} class="whitespace-nowrap" />
{/snippet}
{#snippet actionsCell(profile: Profile)}
	{#if canEdit}
		<span class="flex justify-end gap-1">
			{#if profile.id !== defaultId}
				<button
					type="button"
					class="btn preset-tonal btn-sm"
					onclick={() => setDefault(profile)}
					disabled={settingDefault !== null}
				>
					{#if settingDefault === profile.id}<Spinner />{/if}
					Set as default
				</button>
			{/if}
			{#if !profile.builtin && profile.id !== defaultId}
				<button
					type="button"
					class="btn-icon btn-icon-sm hover:preset-tonal-error"
					onclick={() => askDelete(profile)}
					aria-label="Delete {profile.name}"
				>
					<Trash2Icon />
				</button>
			{/if}
		</span>
	{/if}
{/snippet}

<PageHeader
	title="Profiles"
	description="Platform access and media limits. Assignments apply from the global default down to server, channel and member."
>
	{#snippet actions()}
		{#if canEdit}
			<a
				href={resolve('/(app)/profiles/[id]', { id: 'new' })}
				class="btn preset-filled-primary-500"
			>
				<PlusIcon class="size-4" />
				New profile
			</a>
		{/if}
	{/snippet}
</PageHeader>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<DataTable
		rows={profiles}
		{columns}
		rowKey={(p) => p.id}
		{loading}
		rowHref={(p) => resolve('/(app)/profiles/[id]', { id: p.id })}
		rowLabel={canEdit ? 'Edit' : 'View'}
	/>
{/if}

<Confirm
	bind:open={deleteOpen}
	title="Delete {deleting?.name ?? 'this profile'}?"
	message="Every assignment of it goes away and those scopes inherit again."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
