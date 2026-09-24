<script lang="ts">
	interface Props {
		/** Width and height, in CSS pixels or any CSS length. */
		size?: number | string;
		/** What assistive technology reads. Empty makes the mark decorative. */
		title?: string;
		class?: string;
	}

	let { size = 24, title = 'DiscoClip', class: className = '' }: Props = $props();
</script>

<svg
	viewBox="0 0 100 100"
	width={size}
	height={size}
	class={className}
	fill="currentColor"
	role={title ? 'img' : 'presentation'}
	aria-hidden={title ? undefined : true}
>
	{#if title}<title>{title}</title>{/if}
	<path d="M50 50L88.637 39.647A40 40 0 1 1 60.353 11.363Z" />
	<path d="M57.92 42.08L68.272 3.443A40 40 0 0 1 96.557 31.728Z" />
</svg>
