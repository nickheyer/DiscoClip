<script lang="ts">
	import { languageName, languageOptions } from '$lib/languages';

	interface Props {
		id?: string;
		/** The language tag chosen. `null` picks the `blank` entry. */
		value: string | null;
		/** The wording of the entry that chooses no language, where the API accepts none. */
		blank?: string;
		disabled?: boolean;
		class?: string;
	}

	let {
		id,
		value = $bindable(null),
		blank,
		disabled = false,
		class: className = ''
	}: Props = $props();

	const options = languageOptions();

	/** A tag outside ISO 639-1, such as `pt-BR`, stays selectable as it is. */
	const extra = $derived(
		value !== null && !options.some((option) => option.code === value) ? value : null
	);
</script>

<select
	{id}
	class="select {className}"
	value={value ?? ''}
	onchange={(event) => (value = event.currentTarget.value || null)}
	{disabled}
>
	{#if blank !== undefined}
		<option value="">{blank}</option>
	{/if}
	{#if extra !== null}
		<option value={extra}>{languageName(extra)}</option>
	{/if}
	{#each options as option (option.code)}
		<option value={option.code}>{option.name}</option>
	{/each}
</select>
