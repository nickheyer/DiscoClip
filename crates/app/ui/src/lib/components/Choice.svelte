<script lang="ts" generics="V extends string | boolean">
	import type { Option } from '$lib/policy';

	interface Props {
		id?: string;
		/** The choice made, with undefined picking the inherit entry */
		value: V | undefined;
		options: readonly Option<V>[];
		/** The inherit entry's wording, without which a choice has to be made */
		inherit?: string;
		disabled?: boolean;
		class?: string;
	}

	let {
		id,
		value = $bindable(),
		options,
		inherit,
		disabled = false,
		class: className = ''
	}: Props = $props();

	const key = (v: V) => String(v);

	function pick(chosen: string) {
		value = chosen === '' ? undefined : options.find(([v]) => key(v) === chosen)?.[0];
	}
</script>

<select
	{id}
	class="select {className}"
	value={value === undefined ? '' : key(value)}
	onchange={(event) => pick(event.currentTarget.value)}
	{disabled}
>
	{#if inherit !== undefined}
		<option value="">{inherit}</option>
	{/if}
	{#each options as [v, label] (key(v))}
		<option value={key(v)}>{label}</option>
	{/each}
</select>
