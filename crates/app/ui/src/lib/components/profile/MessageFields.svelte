<script lang="ts">
	import type { EffectivePolicy } from '$lib/api/types';
	import Choice from '$lib/components/Choice.svelte';
	import Field from '$lib/components/Field.svelte';
	import {
		ON_OFF,
		ORIGINAL_EMBEDS,
		ORIGINAL_TEXT,
		PERMISSION_MODES,
		PLACEMENTS,
		REPLACE_AS,
		REQUESTERS,
		inherit,
		labelOf,
		type Option
	} from '$lib/policy';
	import type { ErrorsDraft, MessageDraft, Problems } from '$lib/profile';

	interface Props {
		value: MessageDraft;
		errors: ErrorsDraft;
		/** What the wider scopes give, shown where a choice is left to them */
		effective: EffectivePolicy;
		/** The built-in profile makes every choice, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let { value = $bindable(), errors = $bindable(), effective, builtin, problems }: Props = $props();

	type DestinationChoice = 'same' | 'channel';
	const DESTINATIONS: Option<DestinationChoice>[] = [
		['same', 'Same channel'],
		['channel', 'A channel']
	];

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
	const include = $derived(effective.message.include);
	const onOff = (on: boolean) => inh(labelOf(ON_OFF, on));
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Post results to" for="message-destination" error={problems['message.destination']}>
		<div class="grid gap-2">
			<Choice
				id="message-destination"
				bind:value={
					() => (value.destination === 'inherit' ? undefined : value.destination),
					(chosen) => (value.destination = chosen ?? 'inherit')
				}
				options={DESTINATIONS}
				inherit={inh(effective.message.destination ?? 'Same channel')}
			/>
			{#if value.destination === 'channel'}
				<input
					class="input font-mono"
					type="text"
					inputmode="numeric"
					bind:value={value.channel}
					aria-label="Channel id"
				/>
			{/if}
		</div>
	</Field>
	<Field label="Placement" for="message-placement" error={problems['message.placement']}>
		<Choice
			id="message-placement"
			bind:value={value.placement}
			options={PLACEMENTS}
			inherit={inh(labelOf(PLACEMENTS, effective.message.placement))}
		/>
	</Field>
	<Field
		label="A replacement posts as"
		for="message-replace-as"
		error={problems['message.replace_as']}
	>
		<Choice
			id="message-replace-as"
			bind:value={value.replaceAs}
			options={REPLACE_AS}
			inherit={inh(labelOf(REPLACE_AS, effective.message.replace_as))}
		/>
	</Field>
	<Field
		label="Replaced text"
		for="message-original-text"
		error={problems['message.original_text']}
	>
		<Choice
			id="message-original-text"
			bind:value={value.originalText}
			options={ORIGINAL_TEXT}
			inherit={inh(labelOf(ORIGINAL_TEXT, effective.message.original_text))}
		/>
	</Field>
	<Field
		label="Embeds on the original"
		for="message-original-embeds"
		error={problems['message.original_embeds']}
	>
		<Choice
			id="message-original-embeds"
			bind:value={value.originalEmbeds}
			options={ORIGINAL_EMBEDS}
			inherit={inh(labelOf(ORIGINAL_EMBEDS, effective.message.original_embeds))}
		/>
	</Field>
	<Field label="Bot permissions" for="message-permissions" error={problems['message.permissions']}>
		<Choice
			id="message-permissions"
			bind:value={value.permissions}
			options={PERMISSION_MODES}
			inherit={inh(labelOf(PERMISSION_MODES, effective.message.permissions))}
		/>
	</Field>
	<Field label="Debug failures in the channel" for="errors-debug" error={problems['errors.debug']}>
		<Choice
			id="errors-debug"
			bind:value={errors.debug}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.errors.debug))}
		/>
	</Field>
</div>

<fieldset class="mt-4 grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<legend class="mb-2 label-text">Lines posted with the media</legend>
	<Field label="Source link" for="include-source" error={problems['message.include.source_link']}>
		<Choice
			id="include-source"
			bind:value={value.include.sourceLink}
			options={ON_OFF}
			inherit={onOff(include.source_link)}
		/>
	</Field>
	<Field label="Title" for="include-title" error={problems['message.include.title']}>
		<Choice
			id="include-title"
			bind:value={value.include.title}
			options={ON_OFF}
			inherit={onOff(include.title)}
		/>
	</Field>
	<Field label="Platform" for="include-platform" error={problems['message.include.platform']}>
		<Choice
			id="include-platform"
			bind:value={value.include.platform}
			options={ON_OFF}
			inherit={onOff(include.platform)}
		/>
	</Field>
	<Field label="Uploader" for="include-uploader" error={problems['message.include.uploader']}>
		<Choice
			id="include-uploader"
			bind:value={value.include.uploader}
			options={ON_OFF}
			inherit={onOff(include.uploader)}
		/>
	</Field>
	<Field label="Requester" for="include-requester" error={problems['message.include.requester']}>
		<Choice
			id="include-requester"
			bind:value={value.include.requester}
			options={REQUESTERS}
			inherit={inh(labelOf(REQUESTERS, include.requester))}
		/>
	</Field>
	<Field label="Duration" for="include-duration" error={problems['message.include.duration']}>
		<Choice
			id="include-duration"
			bind:value={value.include.duration}
			options={ON_OFF}
			inherit={onOff(include.duration)}
		/>
	</Field>
	<Field label="Brand" for="include-brand" error={problems['message.include.brand']}>
		<Choice
			id="include-brand"
			bind:value={value.include.brand}
			options={ON_OFF}
			inherit={onOff(include.brand)}
		/>
	</Field>
	<Field
		label="Earlier post"
		for="include-earlier"
		error={problems['message.include.earlier_post']}
	>
		<Choice
			id="include-earlier"
			bind:value={value.include.earlierPost}
			options={ON_OFF}
			inherit={onOff(include.earlier_post)}
		/>
	</Field>
</fieldset>
