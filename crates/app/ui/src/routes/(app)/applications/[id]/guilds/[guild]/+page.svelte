<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import {
		applications,
		guilds as guildsApi,
		profiles as profilesApi,
		rules as rulesApi
	} from '$lib/api/endpoints';
	import type {
		Assignment,
		GuildApplication,
		GuildChannel,
		GuildRole,
		Profile,
		Rule,
		Snowflake
	} from '$lib/api/types';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import Status from '$lib/components/Status.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import AssignmentsPanel from '$lib/components/guild/AssignmentsPanel.svelte';
	import ChannelTree from '$lib/components/guild/ChannelTree.svelte';
	import EffectivePreview from '$lib/components/guild/EffectivePreview.svelte';
	import RuleDialog from '$lib/components/guild/RuleDialog.svelte';
	import { channelLabel } from '$lib/components/guild/channels';
	import { number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? '');
	const guild = $derived(page.params.guild ?? '');

	let apps = $state<GuildApplication[]>([]);
	let channels = $state<GuildChannel[]>([]);
	let roles = $state<GuildRole[]>([]);
	let rules = $state<Rule[]>([]);
	let assignments = $state<Assignment[]>([]);
	let profiles = $state<Profile[]>([]);
	let loading = $state(true);
	let refreshing = $state(false);
	let error = $state<unknown>(null);
	let version = $state(0);

	let dialogOpen = $state(false);
	let editing = $state<Rule | null>(null);
	let editingChannel = $state<Snowflake | null>(null);

	const application = $derived(apps.find((app) => app.application_id === id) ?? null);
	const guildName = $derived(application?.guild_name ?? apps[0]?.guild_name ?? guild);

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [appList, channelList, roleList, ruleList, assignmentList, profileList] =
				await Promise.all([
					guildsApi.applications(guild),
					applications.channels(id, guild),
					applications.roles(id, guild),
					rulesApi.forGuild(id, guild),
					profilesApi.assignments(guild),
					profilesApi.list()
				]);
			if (current !== requestId) return;
			apps = appList;
			channels = channelList;
			roles = roleList;
			rules = ruleList;
			assignments = assignmentList;
			profiles = profileList;
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void id;
		void guild;
		void load();
	});

	async function refresh() {
		refreshing = true;
		try {
			const [channelList, roleList, ruleList] = await Promise.all([
				applications.channels(id, guild),
				applications.roles(id, guild),
				rulesApi.forGuild(id, guild)
			]);
			channels = channelList;
			roles = roleList;
			rules = ruleList;
		} catch (err) {
			reportError(err, 'Could not refresh the server');
		} finally {
			refreshing = false;
		}
	}

	async function reloadAssignments() {
		assignments = await profilesApi.assignments(guild);
		version += 1;
	}

	function openRule(channel: GuildChannel | null, rule: Rule | null) {
		editing = rule;
		editingChannel = channel?.id ?? null;
		dialogOpen = true;
	}

	function onSaved(rule: Rule) {
		const index = rules.findIndex((r) => r.id === rule.id);
		rules = index >= 0 ? rules.map((r) => (r.id === rule.id ? rule : r)) : [...rules, rule];
	}

	function onDeleted(ruleId: string) {
		rules = rules.filter((r) => r.id !== ruleId);
	}

	const ruleColumns: Column<Rule>[] = [
		{
			key: 'channel',
			label: 'Channel',
			value: (rule) =>
				channelLabel(
					channels.find((c) => c.id === rule.channel_id),
					rule.channel_id
				)
		},
		{
			key: 'post_to',
			label: 'Posts to',
			value: (rule) =>
				rule.post_to
					? channelLabel(
							channels.find((c) => c.id === rule.post_to),
							rule.post_to
						)
					: 'Same channel'
		},
		{
			key: 'allowed',
			label: 'Allowed',
			value: (rule) =>
				rule.allow_users.length === 0 && rule.allow_roles.length === 0
					? 'Everyone'
					: `${number(rule.allow_users.length)} members · ${number(rule.allow_roles.length)} roles`
		},
		{ key: 'enabled', label: 'Enabled', cell: enabledCell },
		{ key: 'updated', label: 'Updated', cell: updatedCell },
		{ key: 'edit', label: 'Actions', hideLabel: true, cell: editCell, align: 'right' }
	];
</script>

{#snippet enabledCell(rule: Rule)}
	<Status enabled={rule.enabled} />
{/snippet}
{#snippet updatedCell(rule: Rule)}
	<RelativeTime at={rule.updated_at} class="whitespace-nowrap" />
{/snippet}
{#snippet editCell(rule: Rule)}
	<button
		type="button"
		class="btn preset-tonal btn-sm"
		onclick={() => openRule(channels.find((c) => c.id === rule.channel_id) ?? null, rule)}
	>
		Edit
	</button>
{/snippet}

{#if error && !loading}
	<PageHeader title="Server" />
	<ErrorState {error} title="This server could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Server" />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<a
		href={session.can('manage_applications')
			? resolve('/(app)/applications/[id]', { id })
			: resolve('/servers')}
		class="link-body inline-flex items-center gap-1 text-sm text-surface-700-300 hover:text-surface-950-50"
	>
		<ArrowLeftIcon class="size-4" />
		{session.can('manage_applications') ? (application?.name ?? 'Application') : 'Servers'}
	</a>

	<PageHeader title={guildName}>
		<p class="flex flex-wrap items-center gap-2 text-sm text-surface-600-400">
			<span class="font-mono text-xs">{guild}</span>
			{#if application}
				<span>· {application.name}</span>
				<Status present={application.present} />
			{/if}
		</p>
	</PageHeader>

	<Toolbar>
		<button type="button" class="btn preset-tonal" onclick={refresh} disabled={refreshing}>
			{#if refreshing}<Spinner />{:else}<RefreshCwIcon class="size-4" />{/if}
			Refresh
		</button>
		<button
			type="button"
			class="btn preset-filled-primary-500"
			onclick={() => openRule(null, null)}
		>
			<PlusIcon class="size-4" />
			Add rule
		</button>
	</Toolbar>

	<div class="grid gap-6 lg:grid-cols-5">
		<div class="space-y-6 lg:col-span-3">
			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Channels"
			>
				<h2 class="h6">Channels</h2>
				<ChannelTree {channels} {rules} editable onpick={openRule} />
			</section>

			<section class="space-y-3" aria-label="Watch rules">
				<h2 class="h6">Watch rules ({number(rules.length)})</h2>
				<DataTable rows={rules} columns={ruleColumns} rowKey={(rule) => rule.id} dense>
					{#snippet empty()}
						No channel is watched yet. Pick a channel above to add a rule.
					{/snippet}
				</DataTable>
			</section>
		</div>

		<div class="space-y-6 lg:col-span-2">
			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Profiles"
			>
				<h2 class="h6">Profiles</h2>
				<AssignmentsPanel
					applicationId={id}
					{guild}
					{channels}
					{profiles}
					{assignments}
					onchange={reloadAssignments}
				/>
			</section>

			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Effective settings"
			>
				<h2 class="h6">Effective settings</h2>
				<EffectivePreview applicationId={id} {guild} {channels} {profiles} {version} />
			</section>

			<section
				class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
				aria-label="Roles"
			>
				<h2 class="h6">Roles ({number(roles.length)})</h2>
				<ul class="flex flex-wrap gap-1.5">
					{#each [...roles].sort((a, b) => b.position - a.position) as role (role.id)}
						<li
							class="inline-flex items-center gap-1.5 rounded-full bg-surface-200-800 px-2 py-0.5 text-sm"
							title={role.managed ? 'Managed by an integration' : ''}
						>
							<span
								class="size-2 rounded-full"
								style="background: {role.color === 0
									? 'var(--color-surface-400)'
									: `#${role.color.toString(16).padStart(6, '0')}`}"
								aria-hidden="true"
							></span>
							{role.name}
						</li>
					{/each}
				</ul>
			</section>
		</div>
	</div>
{/if}

<RuleDialog
	bind:open={dialogOpen}
	applicationId={id}
	{guild}
	{channels}
	{roles}
	rule={editing}
	channelId={editingChannel}
	onsaved={onSaved}
	ondeleted={onDeleted}
/>
