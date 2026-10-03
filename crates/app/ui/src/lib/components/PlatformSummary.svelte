<script lang="ts" module>
	import type { PlatformHealth } from '$lib/api/types';
	import type { Tone } from './Status.svelte';

	/** How a platform's verdict reads, and in what tone. */
	export const HEALTH: Record<PlatformHealth, { label: string; tone: Tone }> = {
		working: { label: 'Working', tone: 'success' },
		failing: { label: 'Failing', tone: 'error' },
		login_required: { label: 'Needs login', tone: 'warning' },
		unknown: { label: 'Not checked', tone: 'surface' }
	};

	/** What proves a platform works, newest first: a finished job, or a link that resolved. */
	export function proofOf(platform: {
		last_job_at: string | null;
		last_pass_at: string | null;
	}): { by: 'job' | 'link'; at: string } | null {
		const job = platform.last_job_at;
		const link = platform.last_pass_at;
		if (job && (!link || Date.parse(job) >= Date.parse(link))) return { by: 'job', at: job };
		if (link) return { by: 'link', at: link };
		return null;
	}
</script>

<script lang="ts">
	import type { PlatformCoverage } from '$lib/api/types';
	import MediaKindIcon from './MediaKindIcon.svelte';
	import Timestamp from './Timestamp.svelte';
	import Status from './Status.svelte';
	import { mediaLabel, number } from '$lib/format';

	interface Props {
		platform: PlatformCoverage;
	}

	let { platform }: Props = $props();

	const SESSION = {
		none: 'No login',
		optional: 'Login optional',
		required: 'Login required'
	} as const;

	const health = $derived(
		platform.running ? { label: 'Checking', tone: 'primary' as Tone } : HEALTH[platform.health]
	);

	/** The tags worth a badge: one that repeats a media kind says nothing the kind has not. */
	const tags = $derived.by(() => {
		const kinds = new Set(platform.media.map((kind) => mediaLabel(kind).toLowerCase()));
		return platform.tags.filter((tag) => !kinds.has(tag.toLowerCase()));
	});

	const proof = $derived(proofOf(platform));
	const inUse = $derived(platform.fixtures.filter((f) => f.enabled).length);
</script>

<div class="space-y-3">
	<div class="flex items-start justify-between gap-2">
		<div class="min-w-0">
			<p class="truncate font-semibold">{platform.name}</p>
			<p class="truncate font-mono text-xs text-surface-600-400" title={platform.hosts.join(', ')}>
				{platform.hosts.slice(0, 3).join(', ')}{platform.hosts.length > 3
					? ` +${platform.hosts.length - 3}`
					: ''}
			</p>
		</div>
		<Status label={health.label} tone={health.tone} pulse={platform.running} class="shrink-0" />
	</div>
	<div class="flex flex-wrap gap-1.5">
		{#each platform.media as kind (kind)}
			<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
				<MediaKindIcon {kind} />
				{mediaLabel(kind)}
			</span>
		{/each}
		<span class="badge preset-outlined-surface-300-700" style="--badge-size: var(--text-xs)">
			{SESSION[platform.session]}
		</span>
		{#if platform.cookies > 0}
			<span class="badge preset-outlined-surface-300-700" style="--badge-size: var(--text-xs)">
				{number(platform.cookies)} cookies
			</span>
		{/if}
		{#each tags as tag (tag)}
			<span class="badge preset-outlined-surface-300-700" style="--badge-size: var(--text-xs)">
				{tag}
			</span>
		{/each}
	</div>
	<p class="text-sm text-surface-600-400">
		{#if proof?.by === 'job'}
			Job finished <Timestamp at={proof.at} />
		{:else if proof?.by === 'link'}
			Link resolved <Timestamp at={proof.at} />
		{:else if platform.last_run_at}
			Checked <Timestamp at={platform.last_run_at} />
		{:else}
			Never checked
		{/if}
		· {number(inUse)}
		{inUse === 1 ? 'link' : 'links'}
	</p>
</div>
