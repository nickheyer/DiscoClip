<script lang="ts">
	import PlusIcon from '@lucide/svelte/icons/plus';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { frontends, profiles as profilesApi } from '$lib/api/endpoints';
	import type { Frontend, Profile } from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Status from '$lib/components/Status.svelte';
	import { countText } from '$lib/format';
	import { notify } from '$lib/toast.svelte';

	let views = $state<Frontend[]>([]);
	let profiles = $state<Profile[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let deleting = $state<Frontend | null>(null);
	let deleteOpen = $state(false);

	async function load() {
		loading = true;
		error = null;
		try {
			[views, profiles] = await Promise.all([frontends.list(), profilesApi.list()]);
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	function askDelete(view: Frontend) {
		deleting = view;
		deleteOpen = true;
	}

	async function remove() {
		if (!deleting) return;
		await frontends.remove(deleting.id);
		notify.success('View deleted', deleting.name);
		deleting = null;
		await load();
	}

	function shareUrl(view: Frontend): string {
		return `${location.origin}/f/${view.slug}`;
	}

	function access(view: Frontend): string {
		const a = view.access ?? {};
		const ways: string[] = [];
		if (a.open) return 'Open to everyone';
		if (a.secret_kind)
			ways.push({ pin: 'PIN', password: 'Password', token: 'Token' }[a.secret_kind]);
		if (a.accounts) ways.push('View accounts');
		if (a.providers && a.providers.length > 0) ways.push(a.providers.join(', '));
		return ways.length > 0 ? ways.join(' · ') : 'Closed';
	}

	function scope(view: Frontend): string {
		const s = view.scope ?? {};
		const guilds = s.guilds?.length ?? 0;
		const channels = s.channels?.length ?? 0;
		if (guilds === 0 && channels === 0) return 'Everything';
		return [
			guilds > 0 ? countText(guilds, 'server') : '',
			channels > 0 ? countText(channels, 'channel') : ''
		]
			.filter(Boolean)
			.join(' · ');
	}

	const profileName = (id: string | undefined) =>
		id ? (profiles.find((p) => p.id === id)?.name ?? id) : 'Default';

	function options(view: Frontend): string {
		return [
			view.downloads ? 'Downloads' : null,
			view.links?.enabled ? 'Discord links' : null,
			view.has_secret ? 'Secret set' : null
		]
			.filter((option) => option !== null)
			.join(' · ');
	}

	const columns: Column<Frontend>[] = [
		{ key: 'name', label: 'View', cell: nameCell, sortable: true, value: (v) => v.name },
		{ key: 'scope', label: 'Shows', value: scope },
		{ key: 'access', label: 'Access', value: access },
		{ key: 'profile', label: 'Profile', value: (v) => profileName(v.profile_id) },
		{ key: 'options', label: 'Options', value: options },
		{
			key: 'updated',
			label: 'Updated',
			cell: updatedCell,
			sortable: true,
			value: (v) => v.updated_at
		},
		{ key: 'actions', label: 'Actions', hideLabel: true, cell: actionsCell, align: 'right' }
	];
</script>

{#snippet nameCell(view: Frontend)}
	<div class="min-w-0">
		<p class="flex flex-wrap items-center gap-2 font-medium">
			<span>{view.name}</span>
			{#if !view.enabled}<Status label="Disabled" tone="warning" />{/if}
		</p>
		<p class="flex items-center gap-1 font-mono text-xs text-surface-600-400">
			/f/{view.slug}
			<CopyButton text={shareUrl(view)} label="Copy share link" />
		</p>
	</div>
{/snippet}
{#snippet updatedCell(view: Frontend)}
	<Timestamp at={view.updated_at} class="whitespace-nowrap" />
{/snippet}
{#snippet actionsCell(view: Frontend)}
	<button
		type="button"
		class="btn-icon btn-icon-sm hover:preset-tonal-error"
		onclick={() => askDelete(view)}
		aria-label="Delete {view.name}"
	>
		<Trash2Icon />
	</button>
{/snippet}

<PageHeader
	title="Content views"
	description="Share finished media at /f/<slug> with the audience you choose."
>
	{#snippet actions()}
		<a href={resolve('/(app)/views/[id]', { id: 'new' })} class="btn preset-filled-primary-500">
			<PlusIcon class="size-4" />
			New view
		</a>
	{/snippet}
</PageHeader>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else}
	<DataTable
		rows={views}
		{columns}
		rowKey={(v) => v.id}
		{loading}
		rowHref={(v) => resolve('/(app)/views/[id]', { id: v.id })}
	>
		{#snippet empty()}
			No views yet. Create one to share media outside Discord.
		{/snippet}
	</DataTable>
{/if}

<Confirm
	bind:open={deleteOpen}
	title="Delete {deleting?.name ?? 'this view'}?"
	message="Its accounts and viewer sessions go with it. Shared links stop working."
	confirmLabel="Delete"
	danger
	onconfirm={remove}
/>
