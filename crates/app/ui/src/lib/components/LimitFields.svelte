<script lang="ts">
	import Field from './Field.svelte';
	import type { ProfileLimits } from '$lib/api';
	import { formatBytes, formatDuration } from '$lib/format';

	interface Props {
		id: string;
		value: ProfileLimits;
		disabled?: boolean;
		/** What an empty field means. */
		emptyLabel?: string;
	}

	let { id, value = $bindable(), disabled = false, emptyLabel = 'Inherited' }: Props = $props();

	const BYTE_UNITS = [
		{ label: 'MB', factor: 1024 * 1024 },
		{ label: 'GB', factor: 1024 * 1024 * 1024 }
	];

	// Size and duration are entered in human units and stored as the API wants them.
	let sizeUnit = $state<'MB' | 'GB'>(value.max_source_bytes && value.max_source_bytes >= 1024 ** 3 ? 'GB' : 'MB');
	let sizeText = $state(
		value.max_source_bytes == null
			? ''
			: String(+(value.max_source_bytes / (value.max_source_bytes >= 1024 ** 3 ? 1024 ** 3 : 1024 ** 2)).toFixed(2))
	);
	let durationText = $state(value.max_duration_secs == null ? '' : String(+(value.max_duration_secs / 60).toFixed(2)));
	let heightText = $state(value.max_height == null ? '' : String(value.max_height));

	const sizeProblem = $derived(sizeText !== '' && !(Number(sizeText) > 0) ? 'Enter a size greater than zero.' : null);
	const durationProblem = $derived(
		durationText !== '' && !(Number(durationText) > 0) ? 'Enter a duration greater than zero.' : null
	);
	const heightProblem = $derived(
		heightText !== '' && !(Number.isInteger(Number(heightText)) && Number(heightText) > 0)
			? 'Enter a whole number greater than zero.'
			: null
	);

	$effect(() => {
		const factor = BYTE_UNITS.find((u) => u.label === sizeUnit)!.factor;
		value.max_source_bytes = sizeText === '' || sizeProblem ? null : Math.round(Number(sizeText) * factor);
	});
	$effect(() => {
		value.max_duration_secs = durationText === '' || durationProblem ? null : Math.round(Number(durationText) * 60);
	});
	$effect(() => {
		value.max_height = heightText === '' || heightProblem ? null : Number(heightText);
	});

	export function valid(): boolean {
		return !sizeProblem && !durationProblem && !heightProblem;
	}
</script>

<div class="form-stack">
	<Field
		label="Maximum download size"
		for={`${id}-size`}
		optional
		hint={value.max_source_bytes ? `Downloads over ${formatBytes(value.max_source_bytes)} are refused.` : emptyLabel}
		error={sizeProblem}
	>
		<div class="unit">
			<input
				id={`${id}-size`}
				class="input"
				type="number"
				min="0"
				step="any"
				value={sizeText}
				oninput={(e) => (sizeText = (e.currentTarget as HTMLInputElement).value)}
				placeholder="Inherited"
				{disabled}
				aria-invalid={sizeProblem ? 'true' : undefined}
			/>
			<select class="select" bind:value={sizeUnit} aria-label="Size unit" {disabled}>
				{#each BYTE_UNITS as unit (unit.label)}
					<option value={unit.label}>{unit.label}</option>
				{/each}
			</select>
		</div>
	</Field>
	<Field
		label="Maximum video duration"
		for={`${id}-duration`}
		optional
		hint={value.max_duration_secs ? `Videos over ${formatDuration(value.max_duration_secs)} are refused.` : `In minutes. ${emptyLabel}`}
		error={durationProblem}
	>
		<div class="unit">
			<input
				id={`${id}-duration`}
				class="input"
				type="number"
				min="0"
				step="any"
				value={durationText}
				oninput={(e) => (durationText = (e.currentTarget as HTMLInputElement).value)}
				placeholder="Inherited"
				{disabled}
				aria-invalid={durationProblem ? 'true' : undefined}
			/>
			<span class="unit-label">min</span>
		</div>
	</Field>
	<Field
		label="Maximum output height"
		for={`${id}-height`}
		optional
		hint={value.max_height ? `Sources taller than ${value.max_height}p are downscaled.` : `In pixels. ${emptyLabel}`}
		error={heightProblem}
	>
		<div class="unit">
			<input
				id={`${id}-height`}
				class="input"
				type="number"
				min="0"
				step="1"
				value={heightText}
				oninput={(e) => (heightText = (e.currentTarget as HTMLInputElement).value)}
				placeholder="Inherited"
				{disabled}
				aria-invalid={heightProblem ? 'true' : undefined}
			/>
			<span class="unit-label">px</span>
		</div>
	</Field>
</div>

<style>
	.unit {
		display: grid;
		grid-template-columns: minmax(0, 180px) auto;
		justify-content: start;
		gap: 6px;
		align-items: center;
	}

	.unit .select {
		width: 84px;
	}

	.unit-label {
		color: var(--text-3);
		font-size: 13px;
		padding: 0 6px;
	}
</style>
