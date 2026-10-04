<script lang="ts">
	import { Progress } from '@skeletonlabs/skeleton-svelte';

	interface Props {
		/** The diameter as a CSS variable class, such as `[--size:1.5rem]`. */
		class?: string;
		label?: string;
	}

	let { class: className = '[--size:1rem]', label = 'Loading' }: Props = $props();
</script>

<!-- Skeleton's circular progress in indeterminate mode, drawn in the text colour around it. -->
<Progress value={null} class="inline-flex w-fit shrink-0" aria-label={label}>
	<Progress.Circle class="[--thickness:2px] {className}">
		<Progress.CircleTrack class="stroke-current opacity-25" />
		<Progress.CircleRange class="stroke-current" />
	</Progress.Circle>
</Progress>
