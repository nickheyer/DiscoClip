<script lang="ts">
	import LoaderCircleIcon from '@lucide/svelte/icons/loader-circle';

	interface Props {
		class?: string;
		label?: string;
	}

	let { class: className = 'size-4', label = 'Loading' }: Props = $props();
</script>

<LoaderCircleIcon class="animate-spin {className}" aria-label={label} role="status" />
