<script lang="ts">
	import type { PlatformCoverage } from '$lib/api/types';
	import MediaKindIcon from './MediaKindIcon.svelte';
	import RelativeTime from './RelativeTime.svelte';
	import Status, { type Tone } from './Status.svelte';
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

	const health = $derived.by((): { label: string; tone: Tone } => {
		if (platform.running) return { label: 'Running', tone: 'primary' };
		if (platform.fixtures.length === 0) return { label: 'No links', tone: 'surface' };
		if (platform.failed > 0) return { label: `${number(platform.failed)} failing`, tone: 'error' };
		if (platform.passed === platform.fixtures.length) return { label: 'All pass', tone: 'success' };
		return { label: 'Not run', tone: 'surface' };
	});

	/** The tags worth a badge: one that repeats a media kind says nothing the kind has not. */
	const tags = $derived.by(() => {
		const kinds = new Set(platform.media.map((kind) => mediaLabel(kind).toLowerCase()));
		return platform.tags.filter((tag) => !kinds.has(tag.toLowerCase()));
	});

	/** How the last run went, link by link. */
	const links = $derived.by(() => {
		let text = `${number(platform.passed)} of ${number(platform.fixtures.length)} pass`;
		if (platform.failed > 0) text += ` · ${number(platform.failed)} fail`;
		if (platform.login_required > 0) text += ` · ${number(platform.login_required)} need login`;
		return text;
	});
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
		{#if platform.last_run_at}
			Run <RelativeTime at={platform.last_run_at} />
		{:else}
			Never run
		{/if}
		· {links}
	</p>
</div>
