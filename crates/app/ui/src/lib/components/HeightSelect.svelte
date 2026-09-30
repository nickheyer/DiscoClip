<script lang="ts" module>
	/** The picture heights a limit is chosen among, in pixels. */
	export const HEIGHTS = [144, 240, 360, 480, 720, 1080, 1440, 2160] as const;
</script>

<script lang="ts">
	interface Props {
		id?: string;
		/** The height in pixels. `null` picks the `blank` entry. */
		value: number | null;
		/** The wording of the entry that chooses no height, where the API accepts none. */
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
</script>

<select
	{id}
	class="select {className}"
	value={value === null ? '' : String(value)}
	onchange={(event) =>
		(value = event.currentTarget.value ? Number(event.currentTarget.value) : null)}
	{disabled}
>
	{#if blank !== undefined}
		<option value="">{blank}</option>
	{/if}
	{#each HEIGHTS as height (height)}
		<option value={String(height)}>{height} px</option>
	{/each}
</select>
