<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		class?: string;
		children: Snippet;
	}

	let { class: className = '', children }: Props = $props();
</script>

<dl
	class="grid grid-cols-[minmax(7rem,max-content)_minmax(0,1fr)] items-baseline gap-x-4 gap-y-2 text-sm {className}"
>
	{@render children()}
</dl>
