<script lang="ts">
	import XIcon from '@lucide/svelte/icons/x';
	import { Listbox, useListCollection } from '@skeletonlabs/skeleton-svelte';
	import { applications, rules as rulesApi } from '$lib/api/endpoints';
	import type { GuildChannel, GuildMember, GuildRole, Rule, Snowflake, Uuid } from '$lib/api/types';
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
		/** The rule whose options are edited, or nothing to make a rule. */
		rule: Rule | null;
		/** For a new rule: the channel it watches, or null for every channel of the server. */
		channel?: Snowflake | null;
		/** For a new rule: the rule its options start from. */
		template?: Rule | null;
		onsaved: (rule: Rule) => void;
	}

	let {
		open = $bindable(false),
		applicationId,
		guild,
		channels,
		roles,
		rule,
		channel = null,
		template = null,
		onsaved
	}: Props = $props();

	let postTo = $state('');
	let allowRoles = $state<Snowflake[]>([]);
	let members = $state<{ id: Snowflake; member: GuildMember | null }[]>([]);
	let roleFilter = $state('');
	let saving = $state(false);

	const choices = $derived(watchable(channels));
	/** The channel the options are for, or null for every channel. */
	const target = $derived(rule ? rule.channel_id : channel);
	const label = $derived(
		target === null
			? 'Every channel'
			: channelLabel(
					channels.find((c) => c.id === target),
					target
				)
	);
	const roleList = $derived(
		[...roles]
			.sort((a, b) => b.position - a.position)
			.filter((role) => role.name.toLowerCase().includes(roleFilter.trim().toLowerCase()))
	);
	const roleCollection = $derived(
		useListCollection({
			items: roleList,
			itemToString: (role) => role.name,
			itemToValue: (role) => role.id
		})
	);

	$effect(() => {
		if (!open) return;
		const source = rule ?? template;
		postTo = source?.post_to ?? '';
		allowRoles = [...(source?.allow_roles ?? [])];
		roleFilter = '';
		const ids = source?.allow_users ?? [];
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

	function addMember(member: GuildMember) {
		if (members.some((entry) => entry.id === member.id)) return;
		members = [...members, { id: member.id, member }];
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		saving = true;
		try {
			const input = {
				channel_id: target,
				post_to: postTo || null,
				allow_users: members.map((entry) => entry.id),
				allow_roles: allowRoles,
				enabled: rule?.enabled ?? true
			};
			const saved = rule
				? await rulesApi.update(rule.id, input)
				: await rulesApi.create(applicationId, guild, input);
			notify.success('Options saved', label);
			open = false;
			onsaved(saved);
		} catch (error) {
			reportError(error, 'Could not save the options');
		} finally {
			saving = false;
		}
	}
</script>

<Modal
	bind:open
	title={label}
	description="Where the fetched media goes, and who may post links."
	size="lg"
	busy={saving}
>
	<form id="rule-form" class="space-y-5" onsubmit={save}>
		<Field label="Post results to" for="rule-post-to">
			<select id="rule-post-to" class="select" bind:value={postTo}>
				<option value="">
					{target === null ? 'The channel the link was posted in' : 'The same channel'}
				</option>
				{#each choices as { channel: c, category } (c.id)}
					<option value={c.id}>{category ? `${category} / ` : ''}{channelLabel(c)}</option>
				{/each}
			</select>
		</Field>

		<fieldset class="fieldset space-y-3">
			<legend class="legend">Who may post links</legend>
			<p class="text-sm text-surface-600-400">Nobody chosen means everyone.</p>
			<MemberPicker
				{applicationId}
				{guild}
				placeholder="Add a member"
				exclude={members.map((m) => m.id)}
				onpick={addMember}
			/>
			{#if members.length > 0}
				<ul class="flex flex-wrap gap-2">
					{#each members as entry (entry.id)}
						<li>
							<button
								type="button"
								class="chip preset-tonal"
								aria-label="Remove {entry.member?.username ?? entry.id}"
								onclick={() => (members = members.filter((m) => m.id !== entry.id))}
							>
								{#if entry.member}
									<DiscordAvatar
										user={entry.id}
										hash={entry.member.avatar}
										name={entry.member.username}
										size={18}
									/>
									<span
										>{entry.member.display_name ?? entry.member.nick ?? entry.member.username}</span
									>
								{:else}
									<span class="font-mono text-xs">{entry.id}</span>
								{/if}
								<XIcon />
							</button>
						</li>
					{/each}
				</ul>
			{/if}

			<!-- Skeleton's Listbox with a search box: pick any number of roles. -->
			<Listbox
				collection={roleCollection}
				selectionMode="multiple"
				value={allowRoles}
				onValueChange={(details) => (allowRoles = details.value)}
			>
				<Listbox.Label>Roles</Listbox.Label>
				<Listbox.Input
					placeholder="Filter roles"
					value={roleFilter}
					oninput={(event) => (roleFilter = event.currentTarget.value)}
				/>
				<Listbox.Content class="max-h-48 overflow-y-auto">
					{#if roleCollection.items.length === 0}
						<p class="px-2 py-1 text-sm text-surface-600-400">No roles match.</p>
					{/if}
					{#each roleCollection.items as role (role.id)}
						<Listbox.Item item={role}>
							<Listbox.ItemText class="flex min-w-0 items-center gap-2">
								<span
									class="size-2.5 shrink-0 rounded-full"
									style="background: {roleColor(role)}"
									aria-hidden="true"
								></span>
								<span class="truncate">{role.name}</span>
								{#if role.managed}<span class="text-xs opacity-60">integration</span>{/if}
							</Listbox.ItemText>
							<Listbox.ItemIndicator />
						</Listbox.Item>
					{/each}
				</Listbox.Content>
			</Listbox>
		</fieldset>
	</form>

	{#snippet footer()}
		<button type="button" class="btn preset-tonal" onclick={() => (open = false)} disabled={saving}
			>Cancel</button
		>
		<button type="submit" form="rule-form" class="btn preset-filled-primary-500" disabled={saving}>
			{#if saving}<Spinner />{/if}
			Save
		</button>
	{/snippet}
</Modal>
