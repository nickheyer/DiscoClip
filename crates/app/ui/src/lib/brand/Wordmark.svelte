<script lang="ts">
	import Mark from './Mark.svelte';

	interface Props {
		/** Height of the mark in CSS pixels. The text scales with it. */
		size?: number;
		class?: string;
	}

	let { size = 28, class: className = '' }: Props = $props();
</script>

<span class="inline-flex items-center gap-2 {className}">
	<Mark {size} title="" class="shrink-0 text-primary-500" />
	<span class="leading-none font-bold tracking-[-0.03em]" style="font-size: {size * 0.72}px">
		DiscoClip
	</span>
</span>
