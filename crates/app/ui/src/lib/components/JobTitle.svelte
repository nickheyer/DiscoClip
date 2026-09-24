<script lang="ts">
	import type { JobSummary } from '$lib/api/types';
	import { host } from '$lib/format';
	import MediaKindIcon from './MediaKindIcon.svelte';

	interface Props {
		job: JobSummary;
		/** Show the thumbnail beside the text. */
		thumbnail?: boolean;
	}

	let { job, thumbnail = true }: Props = $props();

	const title = $derived(job.title ?? job.url);
	const subtitle = $derived(
		[job.uploader, job.resolver ?? host(job.url)].filter(Boolean).join(' · ')
	);
</script>

<div class="flex min-w-0 items-center gap-3">
	{#if thumbnail}
		<div
			class="flex h-12 w-20 shrink-0 items-center justify-center overflow-hidden rounded-base bg-surface-200-800 text-surface-600-400"
		>
			{#if job.thumbnail}
				<img src={job.thumbnail} alt="" class="h-full w-full object-cover" loading="lazy" />
			{:else}
				<MediaKindIcon kind={job.media} class="size-5" />
			{/if}
		</div>
	{/if}
	<div class="min-w-0 space-y-1">
		<p class="truncate font-medium" title={job.url}>{title}</p>
		{#if subtitle}
			<p class="truncate text-sm text-surface-600-400">{subtitle}</p>
		{/if}
	</div>
</div>
