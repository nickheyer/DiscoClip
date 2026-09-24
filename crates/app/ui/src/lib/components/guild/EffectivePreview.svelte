<script lang="ts">
	import { profiles as profilesApi } from '$lib/api/endpoints';
	import type {
		EffectiveView,
		GuildChannel,
		GuildMember,
		Profile,
		Snowflake,
		Uuid
	} from '$lib/api/types';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { bytes, clock, number } from '$lib/format';
	import { reportError } from '$lib/toast.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import { channelLabel, watchable } from './channels';

	interface Props {
		applicationId: Uuid;
		guild: Snowflake;
		channels: GuildChannel[];
		profiles: Profile[];
		/** Bumped by the parent when assignments change, so the preview refreshes. */
		version?: number;
	}

	let { applicationId, guild, channels, profiles, version = 0 }: Props = $props();

	let channel = $state('');
	let member = $state<GuildMember | null>(null);
	let view = $state<EffectiveView | null>(null);
	let loading = $state(false);
	let showPlatforms = $state(false);

	const choices = $derived(watchable(channels));
	const profileName = (id: Uuid) => profiles.find((p) => p.id === id)?.name ?? id;

	let requestId = 0;
	async function refresh() {
		const current = ++requestId;
		loading = true;
		try {
			const result = await profilesApi.effective({
				guild,
				channel: channel || undefined,
				user: member?.id
			});
			if (current === requestId) view = result;
		} catch (error) {
			if (current === requestId) reportError(error, 'Could not compute the effective profile');
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void channel;
		void member;
		void version;
		void refresh();
	});

	function scopeLabel(scope: EffectiveView['applied'][number]['scope']): string {
		switch (scope.kind) {
			case 'global':
				return 'Global default';
			case 'guild':
				return 'This server';
			case 'channel':
				return channelLabel(
					channels.find((c) => c.id === scope.channel_id),
					scope.channel_id
				);
			case 'user':
				return `Member ${scope.user_id}`;
		}
	}

	const enabledCount = $derived(view ? Object.values(view.platforms).filter(Boolean).length : 0);
</script>

<div class="space-y-3">
	<div class="grid gap-3 sm:grid-cols-2">
		<label class="label">
			<span class="label-text">Channel</span>
			<select class="select" bind:value={channel}>
				<option value="">Server-wide</option>
				{#each choices as { channel: c, category } (c.id)}
					<option value={c.id}>{category ? `${category} / ` : ''}{channelLabel(c)}</option>
				{/each}
			</select>
		</label>
		<div class="label">
			<span class="label-text">Member</span>
			{#if member}
				<div class="flex items-center gap-2 text-sm">
					<DiscordAvatar user={member.id} hash={member.avatar} name={member.username} size={20} />
					<span class="truncate">{member.display_name ?? member.nick ?? member.username}</span>
					<button
						type="button"
						class="btn btn-sm hover:preset-tonal"
						onclick={() => (member = null)}>Clear</button
					>
				</div>
			{:else}
				<MemberPicker
					{applicationId}
					{guild}
					placeholder="Any member"
					onpick={(picked) => (member = picked)}
				/>
			{/if}
		</div>
	</div>

	{#if view}
		<KeyValue>
			<KeyValueRow
				label="Max source"
				value={view.limits.max_source_bytes === null
					? 'Engine limit'
					: bytes(view.limits.max_source_bytes)}
			/>
			<KeyValueRow
				label="Max duration"
				value={view.limits.max_duration_secs === null
					? 'Engine limit'
					: view.limits.max_duration_secs === 0
						? 'Live streams refused'
						: clock(view.limits.max_duration_secs)}
			/>
			<KeyValueRow
				label="Max height"
				value={view.limits.max_height === null ? 'Engine limit' : `${view.limits.max_height} px`}
			/>
			<KeyValueRow
				label="Platforms"
				value="{number(enabledCount)} on · {number(view.disabled.length)} off"
			/>
			<KeyValueRow label="Applied">
				<ol class="space-y-0.5">
					{#each view.applied as applied, i (i)}
						<li>
							{scopeLabel(applied.scope)} →
							<span class="font-medium">{profileName(applied.profile_id)}</span>
						</li>
					{/each}
				</ol>
			</KeyValueRow>
		</KeyValue>
		{#if view.disabled.length > 0}
			<button
				type="button"
				class="btn preset-tonal btn-sm"
				onclick={() => (showPlatforms = !showPlatforms)}
				aria-expanded={showPlatforms}
			>
				{showPlatforms ? 'Hide' : 'Show'} the {number(view.disabled.length)} platforms turned off
			</button>
			{#if showPlatforms}
				<ul class="flex flex-wrap gap-x-3 gap-y-1 font-mono text-sm">
					{#each view.disabled as id (id)}
						<li>{id}</li>
					{/each}
				</ul>
			{/if}
		{/if}
	{:else if loading}
		<p class="flex items-center gap-2 text-sm text-surface-600-400"><Spinner /> Computing…</p>
	{/if}
</div>
