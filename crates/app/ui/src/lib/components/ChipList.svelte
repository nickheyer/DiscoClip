<script lang="ts">
	interface Props {
		items: readonly string[];
		/** Identifiers, hosts and codes set in the monospace face. */
		mono?: boolean;
		class?: string;
	}

	let { items, mono = false, class: className = '' }: Props = $props();
</script>

{#if items.length > 0}
	<span class="flex flex-wrap gap-1 {className}">
		{#each items as item, i (`${i}:${item}`)}
			<span class="chip preset-tonal {mono ? 'font-mono text-xs' : ''}">{item}</span>
		{/each}
	</span>
{/if}
