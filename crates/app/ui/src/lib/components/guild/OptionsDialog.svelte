<script lang="ts">
	import XIcon from '@lucide/svelte/icons/x';
	import { Listbox, useListCollection } from '@skeletonlabs/skeleton-svelte';
	import { applications, profiles as profilesApi, scopeKey } from '$lib/api/endpoints';
	import type {
		GuildChannel,
		GuildMember,
		GuildRole,
		IntakeOverlay,
		MessageOverlay,
		Profile,
		Scope,
		Snowflake,
		Uuid
	} from '$lib/api/types';
	import Choice from '$lib/components/Choice.svelte';
	import DiscordAvatar from '$lib/components/DiscordAvatar.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { inherit, type Option } from '$lib/policy';
	import { notify, reportError } from '$lib/toast.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import { channelLabel, watchable } from './channels';
	import {
		destinationLabel,
		optionsOf,
		postersLabel,
		settledOptions,
		type Options
	} from './options';

	interface Props {
		open?: boolean;
		applicationId: Uuid;
		guild: Snowflake;
		channels: GuildChannel[];
		roles: GuildRole[];
		/** The place whose options are edited, a channel or the server for every channel */
		scope: Scope;
		/** The profile assigned at the place, whose options the form starts from */
		profile: Profile | null;
		/** Called once the place's own profile holds the options, to read it back */
		onsaved: () => Promise<void>;
	}

	let {
		open = $bindable(false),
		applicationId,
		guild,
		channels,
		roles,
		scope,
		profile,
		onsaved
	}: Props = $props();

	type PostersChoice = 'everyone' | 'chosen';
	const POSTERS: Option<PostersChoice>[] = [
		['everyone', 'Everyone'],
		['chosen', 'Chosen members and roles']
	];

	/** `same`, a channel id, or nothing to inherit */
	let destination = $state<string | undefined>(undefined);
	let posters = $state<PostersChoice | undefined>(undefined);
	let allowRoles = $state<Snowflake[]>([]);
	let members = $state<{ id: Snowflake; member: GuildMember | null }[]>([]);
	let roleFilter = $state('');
	/** What the place gets from the wider scopes, for the inherit entries */
	let inherited = $state<Options | null>(null);
	let saving = $state(false);

	const choices = $derived(watchable(channels));
	const target = $derived(scope.kind === 'channel' ? scope.channel_id : null);
	const label = $derived(
		target === null
			? 'Every channel'
			: channelLabel(
					channels.find((c) => c.id === target),
					target
				)
	);
	const destinations = $derived<Option<string>[]>([
		['same', 'Same channel'],
		...choices.map(({ channel: c, category }): Option<string> => [
			c.id,
			`${category ? `${category} / ` : ''}${channelLabel(c)}`
		])
	]);
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

	let opened = 0;
	$effect(() => {
		if (!open) return;
		const current = ++opened;
		const options = optionsOf(profile);
		destination = options.destination === 'inherit' ? undefined : options.destination;
		posters = options.posters.kind === 'inherit' ? undefined : options.posters.kind;
		allowRoles = options.posters.kind === 'chosen' ? [...options.posters.roles] : [];
		roleFilter = '';
		const ids = options.posters.kind === 'chosen' ? options.posters.users : [];
		members = ids.map((id) => ({ id, member: null }));
		inherited = null;
		profilesApi
			.effective(scope.kind === 'channel' ? { guild } : {})
			.then((effective) => {
				if (current === opened) inherited = settledOptions(effective);
			})
			.catch((error) => reportError(error, 'Could not read the inherited options'));
		for (const id of ids) {
			applications
				.member(applicationId, guild, id)
				.then((found) => {
					if (current !== opened) return;
					members = members.map((entry) => (entry.id === id ? { id, member: found } : entry));
				})
				.catch(() => {
					// The id stays as it is, a member the bot can no longer see
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

	/** The sections to send, the place's own as they are with the options laid over */
	function sections(): { intake: IntakeOverlay; message: MessageOverlay } {
		const base = profile && !profile.builtin ? profile : null;
		const intake: IntakeOverlay = { ...(base?.intake ?? {}) };
		delete intake.allow_users;
		delete intake.allow_roles;
		if (posters === 'everyone') {
			intake.allow_users = [];
			intake.allow_roles = [];
		} else if (posters === 'chosen') {
			intake.allow_users = members.map((entry) => entry.id);
			intake.allow_roles = [...allowRoles];
		}
		const message: MessageOverlay = { ...(base?.message ?? {}) };
		delete message.destination;
		if (destination === 'same') message.destination = null;
		else if (destination !== undefined) message.destination = destination;
		return { intake, message };
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		saving = true;
		try {
			await profilesApi.patchOverlay(scopeKey(scope), sections());
			open = false;
			await onsaved();
			notify.success('Options saved', label);
		} catch (error) {
			reportError(error, 'Could not save the options');
		} finally {
			saving = false;
		}
	}
</script>

<Modal bind:open title={label} size="lg" busy={saving}>
	<form id="options-form" class="space-y-5" onsubmit={save}>
		<div class="grid gap-4 md:grid-cols-2">
			<Field label="Post results to" for="options-destination">
				<Choice
					id="options-destination"
					bind:value={destination}
					options={destinations}
					inherit={inherited
						? inherit(destinationLabel(inherited.destination, channels))
						: 'Inherit'}
				/>
			</Field>
			<Field label="Who may post links" for="options-posters">
				<Choice
					id="options-posters"
					bind:value={posters}
					options={POSTERS}
					inherit={inherited ? inherit(postersLabel(inherited.posters)) : 'Inherit'}
				/>
			</Field>
		</div>

		{#if posters === 'chosen'}
			<div class="grid gap-4 md:grid-cols-2">
				<div class="space-y-3">
					<MemberPicker
						{applicationId}
						{guild}
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
												>{entry.member.display_name ??
													entry.member.nick ??
													entry.member.username}</span
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
				</div>

				<!-- Skeleton's Listbox with a search box, picking any number of roles -->
				<Listbox
					collection={roleCollection}
					selectionMode="multiple"
					value={allowRoles}
					onValueChange={(details) => (allowRoles = details.value)}
				>
					<Listbox.Label>Roles</Listbox.Label>
					<Listbox.Input
						value={roleFilter}
						oninput={(event) => (roleFilter = event.currentTarget.value)}
						aria-label="Filter roles"
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
									{#if role.managed}
										<span class="badge preset-tonal" style="--badge-size: var(--text-xs)"
											>Integration</span
										>
									{/if}
								</Listbox.ItemText>
								<Listbox.ItemIndicator />
							</Listbox.Item>
						{/each}
					</Listbox.Content>
				</Listbox>
			</div>
		{/if}
	</form>

	{#snippet footer()}
		<button type="button" class="btn preset-tonal" onclick={() => (open = false)} disabled={saving}
			>Cancel</button
		>
		<button
			type="submit"
			form="options-form"
			class="btn preset-filled-primary-500"
			disabled={saving}
		>
			{#if saving}<Spinner />{/if}
			Save
		</button>
	{/snippet}
</Modal>
