<script lang="ts">
	import Icon from './Icon.svelte';

	interface Props {
		id: string;
		value: string;
		placeholder?: string;
		autocomplete?: 'current-password' | 'new-password' | 'off';
		required?: boolean;
		invalid?: boolean;
		disabled?: boolean;
		mono?: boolean;
		minlength?: number;
		maxlength?: number;
		oninput?: (event: Event) => void;
	}

	let {
		id,
		value = $bindable(''),
		placeholder,
		autocomplete = 'off',
		required = false,
		invalid = false,
		disabled = false,
		mono = false,
		minlength,
		maxlength,
		oninput
	}: Props = $props();

	let shown = $state(false);
</script>

<div class="wrap">
	<input
		{id}
		class={['input', mono && 'mono']}
		type={shown ? 'text' : 'password'}
		bind:value
		{placeholder}
		{autocomplete}
		{required}
		{disabled}
		{minlength}
		{maxlength}
		{oninput}
		spellcheck="false"
		aria-invalid={invalid ? 'true' : undefined}
	/>
	<button
		type="button"
		class="toggle"
		onclick={() => (shown = !shown)}
		aria-label={shown ? 'Hide' : 'Show'}
		aria-pressed={shown}
		tabindex="-1"
		{disabled}
	>
		<Icon name={shown ? 'eye-off' : 'eye'} size={15} />
	</button>
</div>

<style>
	.wrap {
		position: relative;
	}

	.wrap :global(.input) {
		padding-right: 38px;
	}

	.toggle {
		position: absolute;
		top: 50%;
		right: 6px;
		transform: translateY(-50%);
		display: flex;
		padding: 5px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
	}

	.toggle:hover {
		color: var(--text);
		background: var(--surface-3);
	}
</style>
