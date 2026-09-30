<script lang="ts">
	import { applications, profiles as profilesApi, scopeKey } from '$lib/api/endpoints';
	import type { Assignment, GuildMember, Profile, Snowflake, Uuid } from '$lib/api/types';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { notify, reportError } from '$lib/toast.svelte';
	import MemberPicker from './MemberPicker.svelte';

	interface Props {
		applicationId: Uuid;
		guild: Snowflake;
		profiles: Profile[];
		/** The server's assignments; only the member-scoped ones are shown. */
		assignments: Assignment[];
		onchange: () => Promise<void>;
	}

	let { applicationId, guild, profiles, assignments, onchange }: Props = $props();

	let pending = $state<string | null>(null);
	let newMember = $state<GuildMember | null>(null);
	let newMemberProfile = $state('');
	let memberNames = $state<Record<Snowflake, GuildMember>>({});

	const profileName = (id: Uuid) => profiles.find((p) => p.id === id)?.name ?? id;
	const shownName = (member: GuildMember) => member.display_name ?? member.nick ?? member.username;

	const members = $derived(assignments.filter((a) => a.scope.kind === 'user'));

	$effect(() => {
		for (const assignment of members) {
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
			reportError(error, 'Could not change the member profile');
		} finally {
			pending = null;
		}
	}

	function add() {
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
		void run(key, () => profilesApi.unassign(key), 'Member profile cleared');
	}
</script>

<div class="space-y-3 text-sm">
	{#if members.length > 0}
		<ul class="divide-y divide-surface-200-800">
			{#each members as assignment (scopeKey(assignment.scope))}
				{#if assignment.scope.kind === 'user'}
					{@const key = scopeKey(assignment.scope)}
					{@const member = memberNames[assignment.scope.user_id]}
					<li class="flex items-center gap-2 py-1.5">
						<span class="flex min-w-0 flex-1 items-center gap-2 truncate">
							{#if member}
								<DiscordAvatar
									user={member.id}
									hash={member.avatar}
									name={member.username}
									size={20}
								/>
								{shownName(member)}
							{:else}
								<span class="font-mono text-xs">{assignment.scope.user_id}</span>
							{/if}
							<span class="text-surface-600-400">→</span>
							<span class="font-medium">{profileName(assignment.profile_id)}</span>
						</span>
						<RelativeTime at={assignment.updated_at} class="text-xs text-surface-600-400" />
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
	{:else}
		<p class="text-surface-600-400">No member has a profile of their own yet.</p>
	{/if}
	<div class="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] gap-2">
		<div class="min-w-0">
			{#if newMember}
				<button type="button" class="chip preset-tonal" onclick={() => (newMember = null)}>
					<DiscordAvatar
						user={newMember.id}
						hash={newMember.avatar}
						name={newMember.username}
						size={18}
					/>
					<span class="truncate">{shownName(newMember)}</span>
					<span class="opacity-60">change</span>
				</button>
			{:else}
				<MemberPicker {applicationId} {guild} onpick={(member) => (newMember = member)} />
			{/if}
		</div>
		<select
			class="select"
			bind:value={newMemberProfile}
			aria-label="Profile for the member"
			disabled={pending !== null}
		>
			<option value="" disabled>Profile</option>
			{#each profiles as profile (profile.id)}
				<option value={profile.id}>{profile.name}</option>
			{/each}
		</select>
		<button
			type="button"
			class="btn preset-tonal"
			onclick={add}
			disabled={!newMember || !newMemberProfile || pending !== null}
		>
			Assign
		</button>
	</div>
</div>
