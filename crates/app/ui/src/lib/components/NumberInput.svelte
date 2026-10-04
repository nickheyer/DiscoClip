<script lang="ts">
	interface Props {
		id?: string;
		/** The number, null while the box is empty */
		value: number | null;
		/** The number that applies when the box is left empty, shown in it */
		placeholder?: number | null;
		/** The word beside the box for what the number counts */
		unit?: string;
		min?: number;
		step?: number | 'any';
		disabled?: boolean;
		class?: string;
	}

	let {
		id,
		value = $bindable(null),
		placeholder = null,
		unit,
		min = 0,
		step = 1,
		disabled = false,
		class: className = ''
	}: Props = $props();

	let text = $state('');
	let shown: number | null = null;

	// The box follows outside changes to the value and leaves it alone otherwise, so typing is never overwritten
	$effect(() => {
		if (value === shown) return;
		shown = value;
		text = value === null ? '' : String(value);
	});

	function emit() {
		const trimmed = text.trim();
		if (trimmed === '') {
			shown = null;
			value = null;
			return;
		}
		const n = Number(trimmed);
		if (!Number.isFinite(n)) return;
		shown = n;
		value = n;
	}

	const box =
		'input tabular-nums [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none';
</script>

{#if unit}
	<div class="field-group grid-cols-[1fr_auto] {className}">
		<input
			{id}
			class={box}
			type="number"
			inputmode="numeric"
			{min}
			{step}
			placeholder={placeholder === null ? '' : String(placeholder)}
			bind:value={text}
			oninput={emit}
			{disabled}
		/>
		<div class="label label-text preset-tonal">{unit}</div>
	</div>
{:else}
	<input
		{id}
		class="{box} {className}"
		type="number"
		inputmode="numeric"
		{min}
		{step}
		placeholder={placeholder === null ? '' : String(placeholder)}
		bind:value={text}
		oninput={emit}
		{disabled}
	/>
{/if}
