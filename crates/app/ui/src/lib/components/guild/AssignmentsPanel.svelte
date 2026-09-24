<script lang="ts">
	import { applications, profiles as profilesApi, scopeKey } from '$lib/api/endpoints';
	import type {
		Assignment,
		GuildChannel,
		GuildMember,
		Profile,
		Snowflake,
		Uuid
	} from '$lib/api/types';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { notify, reportError } from '$lib/toast.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import { channelLabel, watchable } from './channels';
	import { resolve } from '$app/paths';

	interface Props {
		applicationId: Uuid;
		guild: Snowflake;
		channels: GuildChannel[];
		profiles: Profile[];
		assignments: Assignment[];
		onchange: () => Promise<void>;
	}

	let { applicationId, guild, channels, profiles, assignments, onchange }: Props = $props();

	let pending = $state<string | null>(null);
	let newChannel = $state('');
	let newChannelProfile = $state('');
	let newMember = $state<GuildMember | null>(null);
	let newMemberProfile = $state('');
	let memberNames = $state<Record<Snowflake, GuildMember>>({});

	const choices = $derived(watchable(channels));
	const profileName = (id: Uuid) => profiles.find((p) => p.id === id)?.name ?? id;

	const global = $derived(assignments.find((a) => a.scope.kind === 'global') ?? null);
	const guildAssignment = $derived(assignments.find((a) => a.scope.kind === 'guild') ?? null);
	const channelAssignments = $derived(assignments.filter((a) => a.scope.kind === 'channel'));
	const userAssignments = $derived(assignments.filter((a) => a.scope.kind === 'user'));

	$effect(() => {
		for (const assignment of userAssignments) {
			if (assignment.scope.kind !== 'user') continue;
			const id = assignment.scope.user_id;
			if (memberNames[id]) continue;
			applications
				.member(applicationId, guild, id)
				.then((member) => (memberNames = { ...memberNames, [id]: member }))
				.catch(() => {
					// The id shows as it is: a member the bot can no longer see.
				});
		}
	});

	async function run(key: string, work: () => Promise<void>, done: string) {
		pending = key;
		try {
			await work();
			notify.success(done);
			await onchange();
		} catch (error) {
			reportError(error, 'Could not change the assignment');
		} finally {
			pending = null;
		}
	}

	function setGuild(profileId: string) {
		const key = scopeKey({ kind: 'guild', guild_id: guild });
		if (profileId) {
			void run(
				key,
				() => profilesApi.assign(key, profileId).then(() => undefined),
				'Server profile set'
			);
		} else {
			void run(key, () => profilesApi.unassign(key), 'Server profile cleared');
		}
	}

	function addChannel() {
		if (!newChannel || !newChannelProfile) return;
		const key = scopeKey({ kind: 'channel', guild_id: guild, channel_id: newChannel });
		const profileId = newChannelProfile;
		void run(
			key,
			() => profilesApi.assign(key, profileId).then(() => undefined),
			'Channel profile set'
		).then(() => {
			newChannel = '';
			newChannelProfile = '';
		});
	}

	function addMember() {
		if (!newMember || !newMemberProfile) return;
		const member = newMember;
		const key = scopeKey({ kind: 'user', guild_id: guild, user_id: member.id });
		const profileId = newMemberProfile;
		memberNames = { ...memberNames, [member.id]: member };
		void run(
			key,
			() => profilesApi.assign(key, profileId).then(() => undefined),
			'Member profile set'
		).then(() => {
			newMember = null;
			newMemberProfile = '';
		});
	}

	function clear(assignment: Assignment) {
		const key = scopeKey(assignment.scope);
		void run(key, () => profilesApi.unassign(key), 'Assignment cleared');
	}
</script>

<div class="space-y-5 text-sm">
	<div class="flex items-center justify-between gap-3">
		<span class="text-surface-600-400">Global default</span>
		<a href={resolve('/profiles')} class="link-body font-medium underline"
			>{global ? profileName(global.profile_id) : 'Default'}</a
		>
	</div>

	<label class="label">
		<span class="label-text">This server</span>
		<select
			class="select"
			value={guildAssignment?.profile_id ?? ''}
			onchange={(event) => setGuild(event.currentTarget.value)}
			disabled={pending !== null}
		>
			<option value="">Inherit the global default</option>
			{#each profiles as profile (profile.id)}
				<option value={profile.id}>{profile.name}</option>
			{/each}
		</select>
	</label>

	<section class="space-y-2" aria-label="Channel assignments">
		<h3 class="label-text">Channels</h3>
		{#if channelAssignments.length > 0}
			<ul class="divide-y divide-surface-200-800 rounded-base border border-surface-200-800">
				{#each channelAssignments as assignment (scopeKey(assignment.scope))}
					{#if assignment.scope.kind === 'channel'}
						{@const key = scopeKey(assignment.scope)}
						{@const channelId = assignment.scope.channel_id}
						<li class="flex items-center gap-2 px-2 py-1.5">
							<span class="min-w-0 flex-1 truncate">
								{channelLabel(
									channels.find((c) => c.id === channelId),
									channelId
								)}
								<span class="text-surface-600-400">→</span>
								<span class="font-medium">{profileName(assignment.profile_id)}</span>
							</span>
							<RelativeTime at={assignment.updated_at} class="text-sm text-surface-600-400" />
							<button
								type="button"
								class="btn btn-sm hover:preset-tonal"
								onclick={() => clear(assignment)}
								disabled={pending !== null}
							>
								{#if pending === key}<Spinner />{/if}
								Clear
							</button>
						</li>
					{/if}
				{/each}
			</ul>
		{/if}
		<div class="flex flex-wrap gap-2">
			<select class="select flex-1" bind:value={newChannel} aria-label="Channel">
				<option value="" disabled>Channel</option>
				{#each choices as { channel: c, category } (c.id)}
					<option value={c.id}>{category ? `${category} / ` : ''}{channelLabel(c)}</option>
				{/each}
			</select>
			<select class="select flex-1" bind:value={newChannelProfile} aria-label="Profile">
				<option value="" disabled>Profile</option>
				{#each profiles as profile (profile.id)}
					<option value={profile.id}>{profile.name}</option>
				{/each}
			</select>
			<button
				type="button"
				class="btn preset-tonal"
				onclick={addChannel}
				disabled={!newChannel || !newChannelProfile || pending !== null}
			>
				Assign
			</button>
		</div>
	</section>

	<section class="space-y-2" aria-label="Member assignments">
		<h3 class="label-text">Members</h3>
		{#if userAssignments.length > 0}
			<ul class="divide-y divide-surface-200-800 rounded-base border border-surface-200-800">
				{#each userAssignments as assignment (scopeKey(assignment.scope))}
					{#if assignment.scope.kind === 'user'}
						{@const key = scopeKey(assignment.scope)}
						{@const member = memberNames[assignment.scope.user_id]}
						<li class="flex items-center gap-2 px-2 py-1.5">
							<span class="flex min-w-0 flex-1 items-center gap-2 truncate">
								{#if member}
									<DiscordAvatar
										user={member.id}
										hash={member.avatar}
										name={member.username}
										size={20}
									/>
									{member.display_name ?? member.nick ?? member.username}
								{:else}
									<span class="font-mono text-xs">{assignment.scope.user_id}</span>
								{/if}
								<span class="text-surface-600-400">→</span>
								<span class="font-medium">{profileName(assignment.profile_id)}</span>
							</span>
							<RelativeTime at={assignment.updated_at} class="text-sm text-surface-600-400" />
							<button
								type="button"
								class="btn btn-sm hover:preset-tonal"
								onclick={() => clear(assignment)}
								disabled={pending !== null}
							>
								{#if pending === key}<Spinner />{/if}
								Clear
							</button>
						</li>
					{/if}
				{/each}
			</ul>
		{/if}
		<div class="flex flex-wrap gap-2">
			<div class="min-w-48 flex-1">
				{#if newMember}
					<div class="flex items-center gap-2">
						<DiscordAvatar
							user={newMember.id}
							hash={newMember.avatar}
							name={newMember.username}
							size={20}
						/>
						<span class="truncate"
							>{newMember.display_name ?? newMember.nick ?? newMember.username}</span
						>
						<button
							type="button"
							class="btn btn-sm hover:preset-tonal"
							onclick={() => (newMember = null)}>Change</button
						>
					</div>
				{:else}
					<MemberPicker {applicationId} {guild} onpick={(member) => (newMember = member)} />
				{/if}
			</div>
			<select class="select flex-1" bind:value={newMemberProfile} aria-label="Profile">
				<option value="" disabled>Profile</option>
				{#each profiles as profile (profile.id)}
					<option value={profile.id}>{profile.name}</option>
				{/each}
			</select>
			<button
				type="button"
				class="btn preset-tonal"
				onclick={addMember}
				disabled={!newMember || !newMemberProfile || pending !== null}
			>
				Assign
			</button>
		</div>
	</section>
</div>
