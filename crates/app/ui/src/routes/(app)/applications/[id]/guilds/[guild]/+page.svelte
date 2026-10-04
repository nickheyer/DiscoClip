<script lang="ts">
	import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import {
		applications,
		guilds as guildsApi,
		profiles as profilesApi,
		rules as rulesApi,
		scopeKey
	} from '$lib/api/endpoints';
	import type {
		Assignment,
		GuildApplication,
		GuildChannel,
		GuildRole,
		Profile,
		Rule,
		Scope,
		Snowflake,
		Uuid
	} from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Identifier from '$lib/components/Identifier.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import ChannelTable from '$lib/components/guild/ChannelTable.svelte';
	import MemberProfiles from '$lib/components/guild/MemberProfiles.svelte';
	import OptionsDialog from '$lib/components/guild/OptionsDialog.svelte';
	import { channelLabel } from '$lib/components/guild/channels';
	import { optionsOf, optionsSummary, profileAt } from '$lib/components/guild/options';
	import { inputOf, stopWatching, watchServer } from '$lib/components/guild/watching';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? '');
	const guild = $derived(page.params.guild ?? '');

	let apps = $state<GuildApplication[]>([]);
	let channels = $state<GuildChannel[]>([]);
	let roles = $state<GuildRole[]>([]);
	let rules = $state<Rule[]>([]);
	let assignments = $state<Assignment[]>([]);
	let profiles = $state<Profile[]>([]);
	let loading = $state(true);
	let error = $state<unknown>(null);

	let dialogOpen = $state(false);
	/** The place whose options the dialog edits */
	let editing = $state<Scope>({ kind: 'guild', guild_id: '' });
	let serverBusy = $state(false);
	let serverPending = $state(false);

	const application = $derived(apps.find((app) => app.application_id === id) ?? null);
	const guildName = $derived(application?.guild_name ?? apps[0]?.guild_name ?? guild);

	const serverRule = $derived(rules.find((rule) => rule.channel_id === null) ?? null);
	const serverOn = $derived(serverRule?.enabled ?? false);
	const serverScope = $derived<Scope>({ kind: 'guild', guild_id: guild });
	const serverProfile = $derived(profileAt(serverScope, assignments, profiles));
	const serverSummary = $derived(
		serverOn ? optionsSummary(optionsOf(serverProfile), channels) : []
	);

	const profileName = (profileId: Uuid) =>
		profiles.find((p) => p.id === profileId)?.name ?? profileId;
	const globalAssignment = $derived(assignments.find((a) => a.scope.kind === 'global') ?? null);
	const guildAssignment = $derived(assignments.find((a) => a.scope.kind === 'guild') ?? null);
	const globalName = $derived(
		globalAssignment ? profileName(globalAssignment.profile_id) : 'Default'
	);
	/** What a channel without a profile of its own gets. */
	const serverName = $derived(
		guildAssignment ? profileName(guildAssignment.profile_id) : globalName
	);
	/** With one profile there is nothing to assign, so the profile controls stay out. */
	const choosable = $derived(profiles.length > 1);
	/** The profile at the place the dialog edits, whose options it starts from */
	const editingProfile = $derived(profileAt(editing, assignments, profiles));

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

	async function reloadAssignments() {
		assignments = await profilesApi.assignments(guild);
	}

	/** Options saved make or change a place's own profile, so both lists are read again */
	async function reloadProfiles() {
		[assignments, profiles] = await Promise.all([
			profilesApi.assignments(guild),
			profilesApi.list()
		]);
	}

	/** Sets or clears one scope's profile. Throws so the control can undo its pick. */
	async function assign(key: string, profileId: Uuid | null, done: string) {
		try {
			if (profileId) await profilesApi.assign(key, profileId);
			else await profilesApi.unassign(key);
			notify.success(done);
			await reloadAssignments();
		} catch (err) {
			reportError(err, 'Could not change the profile');
			throw err;
		}
	}

	function assignChannel(channel: Snowflake, profileId: Uuid | null) {
		const key = scopeKey({ kind: 'channel', guild_id: guild, channel_id: channel });
		return assign(key, profileId, profileId ? 'Channel profile set' : 'Channel profile cleared');
	}

	async function assignServer(event: Event & { currentTarget: HTMLSelectElement }) {
		const select = event.currentTarget;
		const before = guildAssignment?.profile_id ?? '';
		const next = select.value;
		serverPending = true;
		try {
			await assign(
				scopeKey(serverScope),
				next || null,
				next ? 'Server profile set' : 'Server profile cleared'
			);
		} catch {
			select.value = before;
		} finally {
			serverPending = false;
		}
	}

	const labelOf = (channel: Snowflake) =>
		channelLabel(
			channels.find((c) => c.id === channel),
			channel
		);

	function keep(rule: Rule) {
		const index = rules.findIndex((r) => r.id === rule.id);
		rules = index >= 0 ? rules.map((r) => (r.id === rule.id ? rule : r)) : [...rules, rule];
	}

	function drop(ids: Uuid[]) {
		rules = rules.filter((r) => !ids.includes(r.id));
	}

	async function remove(rule: Rule) {
		drop(await stopWatching(rule, rules));
		if (rule.channel_id === null) notify.success('Stopped watching every channel');
		else notify.success('Stopped watching', labelOf(rule.channel_id));
	}

	/** Starts or stops watching a channel. Throws so the switch can settle on the rule as it is. */
	async function watch(channel: Snowflake, on: boolean) {
		const rule = rules.find((r) => r.channel_id === channel) ?? null;
		try {
			if (on) {
				if (rule && !serverOn) {
					keep(await rulesApi.update(rule.id, { ...inputOf(rule), enabled: true }));
				} else if (rule) {
					// A rule that only left the channel out: without it the server's covers the
					// channel again.
					await rulesApi.remove(rule.id);
					drop([rule.id]);
				} else if (!serverOn) {
					keep(await rulesApi.create(id, guild, { channel_id: channel }));
				}
				notify.success('Watching', labelOf(channel));
			} else if (serverOn) {
				// Under the server's rule, off is a rule of the channel's own, switched off.
				keep(
					rule
						? await rulesApi.update(rule.id, { ...inputOf(rule), enabled: false })
						: await rulesApi.create(id, guild, { channel_id: channel, enabled: false })
				);
				notify.success('Left out', labelOf(channel));
			} else if (rule) {
				await remove(rule);
			}
		} catch (err) {
			reportError(err, on ? 'Could not watch the channel' : 'Could not stop watching the channel');
			throw err;
		}
	}

	async function toggleServer(on: boolean) {
		serverBusy = true;
		try {
			if (on) {
				keep(await watchServer(id, guild, serverRule));
				notify.success('Watching every channel');
			} else if (serverRule) {
				await remove(serverRule);
			}
		} catch (err) {
			reportError(err, on ? 'Could not watch the server' : 'Could not stop watching the server');
		} finally {
			serverBusy = false;
		}
	}

	/** Opens the options of a channel, or with null, of the server */
	function openOptions(channel: Snowflake | null) {
		editing =
			channel === null ? serverScope : { kind: 'channel', guild_id: guild, channel_id: channel };
		dialogOpen = true;
	}

	const back = $derived(
		session.can('manage_applications')
			? {
					href: resolve('/(app)/applications/[id]', { id }),
					label: application?.name ?? 'Application'
				}
			: { href: resolve('/'), label: 'Dashboard' }
	);
</script>

{#if error && !loading}
	<PageHeader title="Server" {back} />
	<ErrorState {error} title="This server could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Server" {back} />
	<div class="space-y-3" aria-busy="true">
		<div class="h-8 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader title={guildName} {back}>
		<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-surface-600-400">
			{#if application}
				<Status present={application.present} />
				<span>{application.name}</span>
			{/if}
			<span class="inline-flex items-center gap-1">
				Server id <Identifier value={guild} full label="Copy server id" />
			</span>
		</div>
	</PageHeader>

	<div
		class="grid items-start gap-6 {choosable ? 'lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]' : ''}"
	>
		<Card title="Channels" flush>
			{#snippet actions()}
				<Switch
					checked={serverOn}
					disabled={serverBusy}
					onCheckedChange={(details) => toggleServer(details.checked)}
				>
					<Switch.Control><Switch.Thumb /></Switch.Control>
					<Switch.Label>Watch every channel</Switch.Label>
					<Switch.HiddenInput />
				</Switch>
				{#if serverBusy}
					<Spinner />
				{:else if serverOn}
					<button type="button" class="btn preset-tonal btn-sm" onclick={() => openOptions(null)}>
						<SlidersHorizontalIcon class="size-3.5" />
						Options
					</button>
					{#if serverSummary.length > 0}
						<span class="text-xs text-surface-600-400">{serverSummary.join(' · ')}</span>
					{/if}
				{/if}
				{#if choosable}
					<label class="flex items-center gap-2 text-sm">
						<span class="shrink-0 text-surface-600-400">Server profile</span>
						<select
							class="select w-auto"
							value={guildAssignment?.profile_id ?? ''}
							onchange={assignServer}
							disabled={serverPending}
						>
							<option value="">Global default ({globalName})</option>
							{#each profiles as profile (profile.id)}
								<option value={profile.id}>{profile.name}</option>
							{/each}
						</select>
					</label>
				{/if}
			{/snippet}
			<ChannelTable
				{channels}
				{rules}
				{profiles}
				{assignments}
				inherited={serverName}
				onwatch={watch}
				onoptions={openOptions}
				onassign={assignChannel}
			/>
		</Card>

		{#if choosable}
			<Card title="Member profiles">
				<MemberProfiles
					applicationId={id}
					{guild}
					{profiles}
					{assignments}
					onchange={reloadAssignments}
				/>
			</Card>
		{/if}
	</div>
{/if}

<OptionsDialog
	bind:open={dialogOpen}
	applicationId={id}
	{guild}
	{channels}
	{roles}
	scope={editing}
	profile={editingProfile}
	onsaved={reloadProfiles}
/>
