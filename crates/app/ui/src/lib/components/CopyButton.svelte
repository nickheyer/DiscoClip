<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import { notify } from '$lib/toast.svelte';

	interface Props {
		text: string;
		/** What the button copies, for its label. */
		label?: string;
		/** Show the label beside the icon. */
		withText?: boolean;
		class?: string;
	}

	let { text, label = 'Copy', withText = false, class: className = '' }: Props = $props();

	let copied = $state(false);
	let timer: ReturnType<typeof setTimeout> | null = null;

	async function copy() {
		try {
			await navigator.clipboard.writeText(text);
			copied = true;
			if (timer) clearTimeout(timer);
			timer = setTimeout(() => (copied = false), 1_500);
		} catch (error) {
			notify.error('Could not copy', error instanceof Error ? error.message : String(error));
		}
	}
</script>

<button
	type="button"
	class="{withText ? 'btn btn-sm' : 'btn-icon btn-icon-sm'} hover:preset-tonal {className}"
	onclick={copy}
	title={copied ? 'Copied' : label}
	aria-label={copied ? 'Copied' : label}
>
	{#if copied}
		<CheckIcon class="size-4 text-success-500" />
	{:else}
		<CopyIcon class="size-4" />
	{/if}
	{#if withText}<span>{copied ? 'Copied' : label}</span>{/if}
</button>
