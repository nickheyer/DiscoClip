<script lang="ts">
	import type { EffectivePolicy } from '$lib/api/types';
	import Choice from '$lib/components/Choice.svelte';
	import Field from '$lib/components/Field.svelte';
	import HeightSelect from '$lib/components/HeightSelect.svelte';
	import NumberInput from '$lib/components/NumberInput.svelte';
	import { number } from '$lib/format';
	import {
		AUDIO_CODECS,
		AUDIO_CONTAINERS,
		IMAGE_CONTAINERS,
		VIDEO_CODECS,
		VIDEO_CONTAINERS,
		formatLabel
	} from '$lib/media';
	import { ON_OFF, inherit, labelOf, type Option } from '$lib/policy';
	import type { OutputDraft, Problems } from '$lib/profile';

	interface Props {
		value: OutputDraft;
		/** What the wider scopes give, shown where a choice is left to them */
		effective: EffectivePolicy;
		/** The built-in profile makes every choice, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let { value = $bindable(), effective, builtin, problems }: Props = $props();

	const named = <T extends string>(ids: T[]): Option<T>[] => ids.map((id) => [id, formatLabel(id)]);
	const CONTAINERS = named(VIDEO_CONTAINERS);
	const VIDEO = named(VIDEO_CODECS);
	const AUDIO = named(AUDIO_CODECS);
	type ListChoice = 'chosen';
	const AUDIO_LIST: Option<ListChoice>[] = [['chosen', 'Chosen containers']];
	const IMAGE_LIST: Option<ListChoice>[] = [['chosen', 'Chosen formats']];

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
	const listed = (ids: string[], none: string) =>
		ids.length > 0 ? ids.map(formatLabel).join(', ') : none;
	/** A height, or no cap, which the built-in profile may leave too */
	const capLabel = (height: number | null) => (height === null ? 'No cap' : `${number(height)} px`);

	function toggle<T>(list: T[], item: T): T[] {
		return list.includes(item) ? list.filter((x) => x !== item) : [...list, item];
	}
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Container" for="output-container" error={problems['output.container']}>
		<Choice
			id="output-container"
			bind:value={value.container}
			options={CONTAINERS}
			inherit={inh(formatLabel(effective.output.container))}
		/>
	</Field>
	<Field label="Video codec" for="output-video" error={problems['output.video_codec']}>
		<Choice
			id="output-video"
			bind:value={value.videoCodec}
			options={VIDEO}
			inherit={inh(formatLabel(effective.output.video_codec))}
		/>
	</Field>
	<Field label="Audio codec" for="output-audio-codec" error={problems['output.audio_codec']}>
		<Choice
			id="output-audio-codec"
			bind:value={value.audioCodec}
			options={AUDIO}
			inherit={inh(formatLabel(effective.output.audio_codec))}
		/>
	</Field>
	<Field label="Max height" for="output-height" error={problems['output.max_height']}>
		<HeightSelect
			id="output-height"
			bind:value={value.maxHeight}
			blank={builtin ? 'No cap' : inherit(capLabel(effective.output.max_height))}
		/>
	</Field>
	<Field label="Max frame rate" for="output-fps" error={problems['output.max_fps']}>
		<NumberInput
			id="output-fps"
			bind:value={value.maxFps}
			placeholder={builtin ? null : effective.output.max_fps}
			min={1}
			unit="fps"
		/>
	</Field>
	<Field
		label="Sound over its cover art"
		for="output-still"
		error={problems['output.audio_over_still']}
	>
		<Choice
			id="output-still"
			bind:value={value.audioOverStill}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.output.audio_over_still))}
		/>
	</Field>
	<Field label="Other files" for="output-files" error={problems['output.files']}>
		<Choice
			id="output-files"
			bind:value={value.files}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.output.files))}
		/>
	</Field>
	<Field label="Sound as it is" for="output-audio-list" error={problems['output.audio_containers']}>
		<Choice
			id="output-audio-list"
			bind:value={
				() => (value.audio === 'inherit' ? undefined : value.audio),
				(chosen) => (value.audio = chosen ?? 'inherit')
			}
			options={AUDIO_LIST}
			inherit={inh(listed(effective.output.audio_containers, 'Never'))}
		/>
	</Field>
	{#if value.audio === 'chosen'}
		<fieldset class="flex flex-wrap gap-x-4 gap-y-2 md:col-span-2 xl:col-span-4">
			<legend class="label-text">Sound containers</legend>
			{#each AUDIO_CONTAINERS as container (container)}
				<label class="flex items-center gap-2 text-sm">
					<input
						class="checkbox"
						type="checkbox"
						checked={value.audioContainers.includes(container)}
						onchange={() => (value.audioContainers = toggle(value.audioContainers, container))}
					/>
					{formatLabel(container)}
				</label>
			{/each}
		</fieldset>
	{/if}
	<Field
		label="Images as they are"
		for="output-image-list"
		error={problems['output.image_containers']}
	>
		<Choice
			id="output-image-list"
			bind:value={
				() => (value.images === 'inherit' ? undefined : value.images),
				(chosen) => (value.images = chosen ?? 'inherit')
			}
			options={IMAGE_LIST}
			inherit={inh(listed(effective.output.image_containers, 'Refused'))}
		/>
	</Field>
	{#if value.images === 'chosen'}
		<fieldset class="flex flex-wrap gap-x-4 gap-y-2 md:col-span-2 xl:col-span-4">
			<legend class="label-text">Image formats</legend>
			{#each IMAGE_CONTAINERS as container (container)}
				<label class="flex items-center gap-2 text-sm">
					<input
						class="checkbox"
						type="checkbox"
						checked={value.imageContainers.includes(container)}
						onchange={() => (value.imageContainers = toggle(value.imageContainers, container))}
					/>
					{formatLabel(container)}
				</label>
			{/each}
		</fieldset>
	{/if}
</div>
