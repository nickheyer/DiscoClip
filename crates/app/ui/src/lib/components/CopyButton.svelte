<script lang="ts">
	import Button from './Button.svelte';
	import { toast } from '$lib/state/toast.svelte';

	interface Props {
		text: string;
		label?: string;
		size?: 'sm' | 'md';
		variant?: 'secondary' | 'ghost' | 'primary';
		square?: boolean;
	}

	let { text, label = 'Copy', size = 'sm', variant = 'ghost', square = false }: Props = $props();
	let copied = $state(false);

	async function copy() {
		try {
			await navigator.clipboard.writeText(text);
			copied = true;
			setTimeout(() => (copied = false), 1600);
		} catch {
			toast.error('The browser refused to copy to the clipboard.');
		}
	}
</script>

<Button
	{variant}
	{size}
	{square}
	icon={copied ? 'check' : 'copy'}
	onclick={copy}
	title={copied ? 'Copied' : label}
>
	{#if !square}{copied ? 'Copied' : label}{/if}
</Button>
