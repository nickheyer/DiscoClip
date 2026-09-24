<script lang="ts">
	import XIcon from '@lucide/svelte/icons/x';
	import { applications, rules as rulesApi } from '$lib/api/endpoints';
	import type { GuildChannel, GuildMember, GuildRole, Rule, Snowflake, Uuid } from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { notify, reportError } from '$lib/toast.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import { channelLabel, watchable } from './channels';

	interface Props {
		open?: boolean;
		applicationId: Uuid;
		guild: Snowflake;
		channels: GuildChannel[];
		roles: GuildRole[];
		/** The rule to edit, or nothing for a new one. */
		rule: Rule | null;
		/** The channel a new rule starts on. */
		channelId?: Snowflake | null;
		onsaved: (rule: Rule) => void;
		ondeleted: (id: Uuid) => void;
	}

	let {
		open = $bindable(false),
		applicationId,
		guild,
		channels,
		roles,
		rule,
		channelId = null,
		onsaved,
		ondeleted
	}: Props = $props();

	let channel = $state('');
	let postTo = $state('');
	let allowRoles = $state<Snowflake[]>([]);
	let members = $state<{ id: Snowflake; member: GuildMember | null }[]>([]);
	let enabled = $state(true);
	let roleFilter = $state('');
	let saving = $state(false);
	let confirmDelete = $state(false);

	const choices = $derived(watchable(channels));
	const roleList = $derived(
		[...roles]
			.sort((a, b) => b.position - a.position)
			.filter((role) => role.name.toLowerCase().includes(roleFilter.trim().toLowerCase()))
	);

	$effect(() => {
		if (!open) return;
		const current = rule;
		channel = current?.channel_id ?? channelId ?? '';
		postTo = current?.post_to ?? '';
		allowRoles = [...(current?.allow_roles ?? [])];
		enabled = current?.enabled ?? true;
		roleFilter = '';
		const ids = current?.allow_users ?? [];
		members = ids.map((id) => ({ id, member: null }));
		for (const id of ids) {
			applications
				.member(applicationId, guild, id)
				.then((found) => {
					members = members.map((entry) => (entry.id === id ? { id, member: found } : entry));
				})
				.catch(() => {
					// The id stays as it is: a member the bot can no longer see.
				});
		}
	});

	function roleColor(role: GuildRole): string {
		return role.color === 0
			? 'var(--color-surface-400)'
			: `#${role.color.toString(16).padStart(6, '0')}`;
	}

	function toggleRole(id: Snowflake) {
		allowRoles = allowRoles.includes(id) ? allowRoles.filter((r) => r !== id) : [...allowRoles, id];
	}

	function addMember(member: GuildMember) {
		if (members.some((entry) => entry.id === member.id)) return;
		members = [...members, { id: member.id, member }];
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!channel) return;
		saving = true;
		try {
			const input = {
				channel_id: channel,
				post_to: postTo || null,
				allow_users: members.map((entry) => entry.id),
				allow_roles: allowRoles,
				enabled
			};
			const saved = rule
				? await rulesApi.update(rule.id, input)
				: await rulesApi.create(applicationId, guild, input);
			notify.success(
				rule ? 'Rule saved' : 'Rule added',
				channelLabel(channels.find((c) => c.id === channel))
			);
			open = false;
			onsaved(saved);
		} catch (error) {
			reportError(error, 'Could not save the rule');
		} finally {
			saving = false;
		}
	}

	async function remove() {
		if (!rule) return;
		await rulesApi.remove(rule.id);
		notify.success('Rule removed');
		open = false;
		ondeleted(rule.id);
	}
</script>

<Modal
	bind:open
	title={rule ? 'Edit watch rule' : 'Add watch rule'}
	description="Links posted in the watched channel are fetched and posted back."
	size="lg"
	busy={saving}
>
	<form id="rule-form" class="space-y-5" onsubmit={save}>
		<div class="grid gap-4 sm:grid-cols-2">
			<Field label="Watch channel" for="rule-channel" required>
				<select id="rule-channel" class="select" bind:value={channel} required>
					<option value="" disabled>Choose a channel</option>
					{#each choices as { channel: c, category } (c.id)}
						<option value={c.id}>{category ? `${category} / ` : ''}{channelLabel(c)}</option>
					{/each}
				</select>
			</Field>
			<Field label="Post results to" for="rule-post-to" help="Where the finished media goes.">
				<select id="rule-post-to" class="select" bind:value={postTo}>
					<option value="">The same channel</option>
					{#each choices as { channel: c, category } (c.id)}
						<option value={c.id}>{category ? `${category} / ` : ''}{channelLabel(c)}</option>
					{/each}
				</select>
			</Field>
		</div>

		<fieldset class="space-y-2">
			<legend class="label-text">Allowed members</legend>
			<p class="text-sm text-surface-600-400">
				With no members and no roles, everyone in the channel may post links.
			</p>
			<MemberPicker {applicationId} {guild} exclude={members.map((m) => m.id)} onpick={addMember} />
			{#if members.length > 0}
				<ul class="flex flex-wrap gap-2">
					{#each members as entry (entry.id)}
						<li
							class="flex items-center gap-1.5 rounded-full bg-surface-200-800 py-0.5 pr-1 pl-1 text-sm"
						>
							{#if entry.member}
								<DiscordAvatar
									user={entry.id}
									hash={entry.member.avatar}
									name={entry.member.username}
									size={20}
								/>
								<span
									>{entry.member.display_name ?? entry.member.nick ?? entry.member.username}</span
								>
							{:else}
								<span class="pl-1 font-mono text-xs">{entry.id}</span>
							{/if}
							<button
								type="button"
								class="rounded-full p-0.5 hover:preset-tonal"
								aria-label="Remove member"
								onclick={() => (members = members.filter((m) => m.id !== entry.id))}
							>
								<XIcon class="size-3.5" />
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</fieldset>

		<fieldset class="space-y-2">
			<legend class="label-text">Allowed roles</legend>
			<input
				class="input"
				type="search"
				placeholder="Filter roles"
				aria-label="Filter roles"
				bind:value={roleFilter}
			/>
			<div
				class="max-h-48 space-y-1 overflow-y-auto rounded-base border border-surface-200-800 p-2"
			>
				{#if roleList.length === 0}
					<p class="px-1 text-sm text-surface-600-400">No roles match.</p>
				{/if}
				{#each roleList as role (role.id)}
					<label
						class="flex items-center gap-2 rounded-base px-1 py-0.5 text-sm hover:bg-surface-100-900"
					>
						<input
							class="checkbox"
							type="checkbox"
							checked={allowRoles.includes(role.id)}
							onchange={() => toggleRole(role.id)}
						/>
						<span
							class="size-2.5 rounded-full"
							style="background: {roleColor(role)}"
							aria-hidden="true"
						></span>
						<span class="truncate">{role.name}</span>
						{#if role.managed}<span class="text-surface-600-400">integration</span>{/if}
					</label>
				{/each}
			</div>
		</fieldset>

		<label class="flex items-center gap-2 text-sm">
			<input class="checkbox" type="checkbox" bind:checked={enabled} />
			Rule enabled
		</label>
	</form>

	{#snippet footer()}
		{#if rule}
			<button
				type="button"
				class="mr-auto btn preset-tonal-error"
				onclick={() => (confirmDelete = true)}
				disabled={saving}
			>
				Remove rule
			</button>
		{/if}
		<button type="button" class="btn preset-tonal" onclick={() => (open = false)} disabled={saving}
			>Cancel</button
		>
		<button
			type="submit"
			form="rule-form"
			class="btn preset-filled-primary-500"
			disabled={saving || !channel}
		>
			{#if saving}<Spinner />{/if}
			{rule ? 'Save' : 'Add rule'}
		</button>
	{/snippet}
</Modal>

<Confirm
	bind:open={confirmDelete}
	title="Remove this watch rule?"
	message="The channel stops being watched. Jobs already made stay."
	confirmLabel="Remove"
	danger
	onconfirm={remove}
/>
