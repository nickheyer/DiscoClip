<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import { Steps } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import type { RestoreStatus } from '$lib/api/types';
	import Spinner from '$lib/components/Spinner.svelte';

	let progress = $state<RestoreStatus | null>(null);
	let error = $state('');
	let reconnecting = $state(false);
	let elapsed = $state(0);
	const complete = $derived(progress?.phase === 'complete');
	const failed = $derived(progress?.phase === 'failed');
	const STEPS = [
		'Stop active services',
		'Save current database and restore backup',
		'Restart DiscoClip'
	];
	/** The step under way: the stepper marks every step before it as complete. */
	const step = $derived(
		progress?.phase === 'restoring' ? 1 : progress?.phase === 'starting' ? 2 : complete ? 3 : 0
	);

	onMount(() => {
		const id = page.url.searchParams.get('id');
		if (!id || !/^[\da-f-]{36}$/i.test(id)) {
			error = 'No restore was selected.';
			return;
		}
		let stopped = false;
		let timer: ReturnType<typeof setTimeout>;
		const began = Date.now();
		const controller = new AbortController();
		async function poll() {
			try {
				const response = await fetch(`/api/backups/restore/${encodeURIComponent(id!)}`, {
					cache: 'no-store',
					signal: AbortSignal.any([controller.signal, AbortSignal.timeout(5000)])
				});
				if (stopped) return;
				if (response.status === 404) {
					error = 'This restore receipt is no longer available. Sign in to check your backups.';
					return;
				}
				if (!response.ok) throw new Error('Reconnecting');
				progress = (await response.json()) as RestoreStatus;
				reconnecting = false;
				if (progress.phase === 'complete' || progress.phase === 'failed') return;
			} catch {
				if (stopped) return;
				reconnecting = true;
			}
			elapsed = Date.now() - began;
			if (!stopped) timer = setTimeout(() => void poll(), 1500);
		}
		void poll();
		return () => {
			stopped = true;
			clearTimeout(timer);
			controller.abort();
		};
	});
</script>

<svelte:head><title>Restore backup · DiscoClip</title></svelte:head>

<div class="space-y-4" aria-live="polite">
	{#if complete}
		<CheckIcon class="size-6 text-success-600-400" />
		<h1 class="h4">Backup restored</h1>
		<p class="text-surface-600-400">
			DiscoClip is ready. Sign in with an account from the restored backup.
		</p>
		<p class="text-sm text-surface-600-400">Your previous database is saved in Backups.</p>
	{:else if failed || error}
		<h1 class="h4">{failed ? 'Restore could not finish' : 'Restore status unavailable'}</h1>
		<p class="card preset-tonal-error p-3 text-sm" role="alert">{progress?.error ?? error}</p>
	{:else}
		<Spinner class="[--size:1.5rem]" />
		<h1 class="h4">Restoring your backup</h1>
		<p class="text-surface-600-400">
			{reconnecting
				? 'Waiting for DiscoClip to reconnect…'
				: 'DiscoClip will reconnect automatically.'}
		</p>
		<!-- Skeleton's Steps, read-only: the restore moves through them on its own. -->
		<Steps count={STEPS.length} {step} orientation="vertical" linear class="py-2">
			<Steps.List>
				{#each STEPS as label, index (label)}
					<Steps.Item {index}>
						<Steps.Trigger class="pointer-events-none text-left" tabindex={-1}>
							<Steps.Indicator>
								{#if index < step}
									<CheckIcon class="size-4" />
								{:else}
									{index + 1}
								{/if}
							</Steps.Indicator>
							<span class="text-sm {index === step ? '' : 'text-surface-600-400'}">{label}</span>
						</Steps.Trigger>
						{#if index < STEPS.length - 1}
							<Steps.Separator />
						{/if}
					</Steps.Item>
				{/each}
			</Steps.List>
		</Steps>
		{#if elapsed > 60000}
			<p class="text-sm text-surface-600-400">
				Still working. Active jobs may need time to finish before the database can be restored.
			</p>
		{/if}
	{/if}
	{#if complete || failed || error}
		<a class="btn w-full preset-filled-primary-500" href={resolve('/login')}>Continue to sign in</a>
	{/if}
</div>
