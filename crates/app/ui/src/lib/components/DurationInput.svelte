<script lang="ts">
	interface Props {
		id?: string;
		/** Seconds. `null` while every segment is empty. */
		value: number | null;
		/** The seconds that apply when the segments are left empty, shown in them. */
		placeholder?: number | null;
		disabled?: boolean;
		/** The segments' shared label, for assistive technology. */
		label?: string;
		class?: string;
	}

	let {
		id,
		value = $bindable(null),
		placeholder = null,
		disabled = false,
		label = 'Duration',
		class: className = ''
	}: Props = $props();

	/** The hour, minute and second parts of a number of seconds. */
	function split(secs: number): [number, number, number] {
		const total = Math.max(0, Math.round(secs));
		return [Math.floor(total / 3600), Math.floor((total % 3600) / 60), total % 60];
	}

	let hours = $state('');
	let minutes = $state('');
	let seconds = $state('');
	let shown: number | null = null;

	// The segments follow the value when it changes from outside, and leave the value
	// alone otherwise, so typing is never overwritten.
	$effect(() => {
		if (value === shown) return;
		shown = value;
		if (value === null) {
			hours = '';
			minutes = '';
			seconds = '';
		} else {
			const [h, m, s] = split(value);
			hours = h === 0 ? '' : String(h);
			minutes = String(m);
			seconds = String(s);
		}
	});

	function emit() {
		const parts = [hours, minutes, seconds].map((part) => part.trim());
		if (parts.every((part) => part === '')) {
			shown = null;
			value = null;
			return;
		}
		const [h, m, s] = parts.map((part) => (part === '' ? 0 : Number(part)));
		if (![h, m, s].every((n) => Number.isFinite(n) && n >= 0)) return;
		shown = Math.round(h * 3600 + m * 60 + s);
		value = shown;
	}

	const hint = $derived(placeholder === null ? null : split(placeholder));

	const segment =
		'input text-center tabular-nums [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none';
</script>

<div
	class="field-group grid-cols-[1fr_auto_1fr_auto_1fr] {className}"
	role="group"
	aria-label={label}
>
	<input
		{id}
		class={segment}
		type="number"
		inputmode="numeric"
		min="0"
		step="1"
		placeholder={hint ? String(hint[0]) : ''}
		aria-label="Hours"
		bind:value={hours}
		oninput={emit}
		{disabled}
	/>
	<div class="label label-text preset-tonal">h</div>
	<input
		class={segment}
		type="number"
		inputmode="numeric"
		min="0"
		max="59"
		step="1"
		placeholder={hint ? String(hint[1]).padStart(2, '0') : ''}
		aria-label="Minutes"
		bind:value={minutes}
		oninput={emit}
		{disabled}
	/>
	<div class="label label-text preset-tonal">m</div>
	<input
		class={segment}
		type="number"
		inputmode="numeric"
		min="0"
		max="59"
		step="1"
		placeholder={hint ? String(hint[2]).padStart(2, '0') : ''}
		aria-label="Seconds"
		bind:value={seconds}
		oninput={emit}
		{disabled}
	/>
	<div class="label label-text preset-tonal">s</div>
</div>
