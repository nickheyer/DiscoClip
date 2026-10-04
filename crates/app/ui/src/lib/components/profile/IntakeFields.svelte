<script lang="ts">
	import type { EffectivePolicy } from '$lib/api/types';
	import Choice from '$lib/components/Choice.svelte';
	import Field from '$lib/components/Field.svelte';
	import IdTags from '$lib/components/IdTags.svelte';
	import NumberInput from '$lib/components/NumberInput.svelte';
	import { postersLabel, settledOptions } from '$lib/components/guild/options';
	import { BOT_MESSAGES, ON_OFF, inherit, labelOf, type Option } from '$lib/policy';
	import type { IntakeDraft, Problems } from '$lib/profile';

	interface Props {
		value: IntakeDraft;
		/** What the wider scopes give, shown where a choice is left to them */
		effective: EffectivePolicy;
		/** The built-in profile makes every choice, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let { value = $bindable(), effective, builtin, problems }: Props = $props();

	type PostersChoice = 'everyone' | 'chosen';
	const POSTERS: Option<PostersChoice>[] = [
		['everyone', 'Everyone'],
		['chosen', 'Chosen members and roles']
	];

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
	const inheritedPosters = $derived(postersLabel(settledOptions(effective).posters));
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Who may post links" for="intake-posters" error={problems['intake.allow_users']}>
		<Choice
			id="intake-posters"
			bind:value={
				() => (value.posters === 'inherit' ? undefined : value.posters),
				(chosen) => (value.posters = chosen ?? 'inherit')
			}
			options={POSTERS}
			inherit={inh(inheritedPosters)}
		/>
	</Field>
	<Field label="Messages from bots" for="intake-bots" error={problems['intake.bot_messages']}>
		<Choice
			id="intake-bots"
			bind:value={value.botMessages}
			options={BOT_MESSAGES}
			inherit={inh(labelOf(BOT_MESSAGES, effective.intake.bot_messages))}
		/>
	</Field>
	<Field label="Live streams" for="intake-live" error={problems['intake.live']}>
		<Choice
			id="intake-live"
			bind:value={value.live}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.intake.live))}
		/>
	</Field>
	<Field label="Playlists" for="intake-playlists" error={problems['intake.playlists.enabled']}>
		<Choice
			id="intake-playlists"
			bind:value={value.playlists}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.intake.playlists.enabled))}
		/>
	</Field>
	<Field
		label="Max playlist entries"
		for="intake-entries"
		error={problems['intake.playlists.max_entries']}
	>
		<NumberInput
			id="intake-entries"
			bind:value={value.maxEntries}
			placeholder={builtin ? null : effective.intake.playlists.max_entries}
			min={1}
			unit="entries"
		/>
	</Field>
	{#if value.posters === 'chosen'}
		<Field
			label="Members"
			for="intake-users"
			error={problems['intake.allow_users']}
			class="md:col-span-2"
		>
			<IdTags id="intake-users" bind:value={value.users} label="Member ids" />
		</Field>
		<Field
			label="Roles"
			for="intake-roles"
			error={problems['intake.allow_roles']}
			class="md:col-span-2"
		>
			<IdTags id="intake-roles" bind:value={value.roles} label="Role ids" />
		</Field>
	{/if}
</div>
