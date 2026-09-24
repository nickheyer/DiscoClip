<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		class?: string;
		children: Snippet;
	}

	let { class: className = '', children }: Props = $props();
</script>

<dl
	class="grid grid-cols-[minmax(0,auto)_minmax(0,1fr)] items-baseline gap-x-6 gap-y-3 text-sm {className}"
>
	{@render children()}
</dl>
