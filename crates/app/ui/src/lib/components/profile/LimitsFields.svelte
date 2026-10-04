<script lang="ts">
	import type { EffectivePolicy } from '$lib/api/types';
	import BytesInput from '$lib/components/BytesInput.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import DurationInput from '$lib/components/DurationInput.svelte';
	import Field from '$lib/components/Field.svelte';
	import HeightSelect from '$lib/components/HeightSelect.svelte';
	import { durationText, number } from '$lib/format';
	import { inherit, type Option } from '$lib/policy';
	import type { LimitsDraft, Problems } from '$lib/profile';

	interface Props {
		value: LimitsDraft;
		/** What the wider scopes give, shown where a limit is left to them */
		effective: EffectivePolicy;
		/** The built-in profile names every limit, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let { value = $bindable(), effective, builtin, problems }: Props = $props();

	type DurationChoice = 'none' | 'limit';
	const DURATIONS: Option<DurationChoice>[] = [
		['none', 'No limit'],
		['limit', 'Limit']
	];

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
	function hint<T>(shown: T): T | null {
		return builtin ? null : shown;
	}
	const inheritedDuration = $derived(
		effective.limits.max_duration_secs === null
			? 'No limit'
			: durationText(effective.limits.max_duration_secs)
	);
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Max source size" for="limits-size" error={problems['limits.max_source_bytes']}>
		<BytesInput
			id="limits-size"
			bind:value={value.maxSourceBytes}
			placeholder={hint(effective.limits.max_source_bytes)}
		/>
	</Field>
	<Field label="Max duration" for="limits-duration" error={problems['limits.max_duration_secs']}>
		<div class="grid gap-2">
			<Choice
				id="limits-duration"
				bind:value={
					() => (value.duration === 'inherit' ? undefined : value.duration),
					(chosen) => (value.duration = chosen ?? 'inherit')
				}
				options={DURATIONS}
				inherit={inh(inheritedDuration)}
			/>
			{#if value.duration === 'limit'}
				<DurationInput bind:value={value.maxDurationSecs} label="Max duration" />
			{/if}
		</div>
	</Field>
	<Field label="Max height" for="limits-height" error={problems['limits.max_height']}>
		<HeightSelect
			id="limits-height"
			bind:value={value.maxHeight}
			blank={inh(`${number(effective.limits.max_height)} px`)}
		/>
	</Field>
	<Field
		label="Max capture length"
		for="limits-capture"
		error={problems['limits.max_capture_secs']}
	>
		<DurationInput
			id="limits-capture"
			bind:value={value.maxCaptureSecs}
			placeholder={hint(effective.limits.max_capture_secs)}
			label="Max capture length"
		/>
	</Field>
</div>
