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
		[job.uploader, job.platform ?? host(job.url)].filter(Boolean).join(' · ')
	);
</script>

<div class="flex min-w-0 items-center gap-3">
	{#if thumbnail}
		<div
			class="flex h-9 w-14 shrink-0 items-center justify-center overflow-hidden rounded-base preset-tonal"
		>
			{#if job.thumbnail}
				<img src={job.thumbnail} alt="" class="h-full w-full object-cover" loading="lazy" />
			{:else}
				<MediaKindIcon kind={job.media} class="size-4" />
			{/if}
		</div>
	{/if}
	<div class="min-w-0">
		<p class="truncate font-medium" title={job.url}>{title}</p>
		{#if subtitle}
			<p class="truncate text-xs text-surface-600-400">{subtitle}</p>
		{/if}
	</div>
</div>
