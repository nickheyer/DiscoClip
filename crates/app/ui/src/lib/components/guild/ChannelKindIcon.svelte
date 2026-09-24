<script lang="ts">
	import FolderIcon from '@lucide/svelte/icons/folder';
	import HashIcon from '@lucide/svelte/icons/hash';
	import ImageIcon from '@lucide/svelte/icons/image';
	import MegaphoneIcon from '@lucide/svelte/icons/megaphone';
	import MessageSquareIcon from '@lucide/svelte/icons/message-square';
	import MessagesSquareIcon from '@lucide/svelte/icons/messages-square';
	import RadioIcon from '@lucide/svelte/icons/radio';
	import Volume2Icon from '@lucide/svelte/icons/volume-2';
	import CircleIcon from '@lucide/svelte/icons/circle';
	import type { ChannelKind } from '$lib/api/types';

	interface Props {
		kind: ChannelKind;
		class?: string;
	}

	let { kind, class: className = 'size-4' }: Props = $props();
</script>

{#if kind === 'text'}
	<HashIcon class={className} />
{:else if kind === 'announcement'}
	<MegaphoneIcon class={className} />
{:else if kind === 'voice'}
	<Volume2Icon class={className} />
{:else if kind === 'stage'}
	<RadioIcon class={className} />
{:else if kind === 'category'}
	<FolderIcon class={className} />
{:else if kind === 'forum'}
	<MessagesSquareIcon class={className} />
{:else if kind === 'media'}
	<ImageIcon class={className} />
{:else if kind === 'thread'}
	<MessageSquareIcon class={className} />
{:else}
	<CircleIcon class={className} />
{/if}
