<script lang="ts" module>
	import type { JobStatus, Stage, StatusKind } from '$lib/api';

	export const STATUS_LABELS: Record<StatusKind, string> = {
		queued: 'Queued',
		running: 'Running',
		done: 'Done',
		failed: 'Failed',
		cancelled: 'Cancelled'
	};

	export const STAGE_LABELS: Record<Stage, string> = {
		resolve: 'Resolving',
		download: 'Downloading',
		transcode: 'Transcoding',
		publish: 'Publishing',
		archive: 'Archiving'
	};

	export function statusTone(
		status: StatusKind
	): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent' {
		switch (status) {
			case 'done':
				return 'ok';
			case 'running':
				return 'info';
			case 'failed':
				return 'danger';
			case 'cancelled':
				return 'warn';
			default:
				return 'neutral';
		}
	}

	export function statusLabel(status: JobStatus): string {
		if (status.status === 'running') return STAGE_LABELS[status.stage];
		if (status.status === 'failed') return `Failed while ${STAGE_LABELS[status.stage].toLowerCase()}`;
		return STATUS_LABELS[status.status];
	}
</script>

<script lang="ts">
	import Badge from './Badge.svelte';

	let { status, size = 'md', short = false }: { status: JobStatus; size?: 'sm' | 'md'; short?: boolean } =
		$props();
	const live = $derived(status.status === 'running');
	const label = $derived(
		short && status.status === 'failed' ? STATUS_LABELS.failed : statusLabel(status)
	);
</script>

<Badge tone={statusTone(status.status)} dot pulse={live} {size} title={status.status === 'failed' ? status.message : undefined}>{label}</Badge>
