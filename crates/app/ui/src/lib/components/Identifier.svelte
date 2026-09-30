<script lang="ts">
	import CopyButton from './CopyButton.svelte';
	import { shortId } from '$lib/format';

	interface Props {
		value: string;
		/** What the copy button copies, for its label. */
		label?: string;
		/** Where the identifier leads, when it names a page. */
		href?: string;
		/** Show the whole identifier rather than its first block. */
		full?: boolean;
		class?: string;
	}

	let { value, label = 'Copy id', href, full = false, class: className = '' }: Props = $props();

	const shown = $derived(full ? value : shortId(value));
</script>

<span class="inline-flex min-w-0 items-center gap-1 {className}">
	{#if href}
		<a {href} class="truncate anchor font-mono text-xs" title={value}>{shown}</a>
	{:else}
		<span class="truncate font-mono text-xs" title={value}>{shown}</span>
	{/if}
	<CopyButton text={value} {label} />
</span>
