<script lang="ts">
	type Unit = 'MB' | 'GB';

	interface Props {
		id?: string;
		/** Bytes. `null` while the number is empty. */
		value: number | null;
		/** The bytes that apply when the number is left empty, shown in it. */
		placeholder?: number | null;
		disabled?: boolean;
		class?: string;
	}

	let {
		id,
		value = $bindable(null),
		placeholder = null,
		disabled = false,
		class: className = ''
	}: Props = $props();

	const MULTIPLIER: Record<Unit, number> = { MB: 1024 * 1024, GB: 1024 * 1024 * 1024 };

	/** The unit a number of bytes reads best in. */
	function unitOf(bytes: number): Unit {
		return bytes >= MULTIPLIER.GB ? 'GB' : 'MB';
	}

	/** A number of bytes in `unit`, to two decimals. */
	function inUnit(bytes: number, unit: Unit): string {
		return String(Math.round((bytes / MULTIPLIER[unit]) * 100) / 100);
	}

	let unit = $state<Unit>('MB');
	let text = $state('');
	let shown: number | null = null;

	// The number follows the value when it changes from outside, and leaves the value
	// alone otherwise, so typing is never overwritten.
	$effect(() => {
		if (value === shown) return;
		shown = value;
		if (value === null) {
			text = '';
			unit = placeholder === null ? 'MB' : unitOf(placeholder);
		} else {
			unit = unitOf(value);
			text = inUnit(value, unit);
		}
	});

	function emit() {
		const trimmed = text.trim();
		if (trimmed === '') {
			shown = null;
			value = null;
			return;
		}
		const n = Number(trimmed);
		if (!Number.isFinite(n) || n < 0) return;
		shown = Math.round(n * MULTIPLIER[unit]);
		value = shown;
	}

	function changeUnit(next: Unit) {
		unit = next;
		emit();
	}

	const hint = $derived(placeholder === null ? '' : inUnit(placeholder, unit));
</script>

<div class="field-group grid-cols-[1fr_auto] {className}">
	<input
		{id}
		class="input [appearance:textfield] tabular-nums [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
		type="number"
		inputmode="decimal"
		min="0"
		step="any"
		placeholder={hint}
		bind:value={text}
		oninput={emit}
		{disabled}
	/>
	<select
		class="select w-20"
		aria-label="Unit"
		value={unit}
		onchange={(event) => changeUnit(event.currentTarget.value as Unit)}
		{disabled}
	>
		<option value="MB">MB</option>
		<option value="GB">GB</option>
	</select>
</div>
