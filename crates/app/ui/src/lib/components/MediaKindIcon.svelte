<script lang="ts">
	import FileIcon from '@lucide/svelte/icons/file';
	import FilmIcon from '@lucide/svelte/icons/film';
	import ImageIcon from '@lucide/svelte/icons/image';
	import MusicIcon from '@lucide/svelte/icons/music';
	import type { MediaKind } from '$lib/api/types';
	import { mediaLabel } from '$lib/format';

	interface Props {
		kind: MediaKind;
		class?: string;
	}

	let { kind, class: className = 'size-4' }: Props = $props();
</script>

{#if kind === 'video'}
	<FilmIcon class={className} aria-label={mediaLabel(kind)} />
{:else if kind === 'audio'}
	<MusicIcon class={className} aria-label={mediaLabel(kind)} />
{:else if kind === 'image'}
	<ImageIcon class={className} aria-label={mediaLabel(kind)} />
{:else}
	<FileIcon class={className} aria-label={mediaLabel(kind)} />
{/if}
