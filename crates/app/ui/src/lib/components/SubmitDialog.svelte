<script lang="ts">
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import { Collapsible } from '@skeletonlabs/skeleton-svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { jobs } from '$lib/api/endpoints';
	import type { SubmitRequest, SubtitleMode } from '$lib/api/types';
	import { parseClock, toDuration } from '$lib/format';
	import { notify, reportError } from '$lib/toast.svelte';
	import Field from './Field.svelte';
	import LanguageSelect from './LanguageSelect.svelte';
	import Modal from './Modal.svelte';
	import Spinner from './Spinner.svelte';

	interface Props {
		open?: boolean;
	}

	let { open = $bindable(false) }: Props = $props();

	let url = $state('');
	let maxSourceMb = $state('');
	let maxDuration = $state('');
	let maxHeight = $state('');
	let clipStart = $state('');
	let clipEnd = $state('');
	let subtitles = $state<SubtitleMode>('keep');
	let subtitleLanguage = $state('');
	let audioLanguage = $state<string | null>('en');
	let advanced = $state(false);
	let pending = $state(false);
	let errors = $state<Record<string, string>>({});

	function reset() {
		url = '';
		maxSourceMb = '';
		maxDuration = '';
		maxHeight = '';
		clipStart = '';
		clipEnd = '';
		subtitles = 'keep';
		subtitleLanguage = '';
		audioLanguage = 'en';
		errors = {};
	}

	function build(): SubmitRequest | null {
		const found: Record<string, string> = {};
		let link: URL | null;
		try {
			link = new URL(url.trim());
		} catch {
			link = null;
		}
		if (!link || (link.protocol !== 'http:' && link.protocol !== 'https:')) {
			found.url = 'Enter an http or https link.';
		}
		const request: SubmitRequest = { url: url.trim() };

		const limits: NonNullable<SubmitRequest['limits']> = {
			max_source_bytes: null,
			max_duration_secs: null,
			max_height: null,
			max_capture_secs: null
		};
		if (maxSourceMb.trim()) {
			const mb = Number(maxSourceMb);
			if (!Number.isFinite(mb) || mb <= 0) found.maxSourceMb = 'Enter a size above zero.';
			else limits.max_source_bytes = Math.round(mb * 1024 * 1024);
		}
		if (maxDuration.trim()) {
			const secs = parseClock(maxDuration);
			if (secs === null || secs < 0) found.maxDuration = 'Enter seconds or h:mm:ss.';
			else limits.max_duration_secs = Math.round(secs);
		}
		if (maxHeight.trim()) {
			const px = Number(maxHeight);
			if (!Number.isInteger(px) || px <= 0) found.maxHeight = 'Enter whole pixels above zero.';
			else limits.max_height = px;
		}
		if (
			limits.max_source_bytes !== null ||
			limits.max_duration_secs !== null ||
			limits.max_height !== null
		) {
			request.limits = limits;
		}

		let start: number | null = null;
		let end: number | null = null;
		if (clipStart.trim()) {
			start = parseClock(clipStart);
			if (start === null) found.clipStart = 'Enter seconds or h:mm:ss.';
		}
		if (clipEnd.trim()) {
			end = parseClock(clipEnd);
			if (end === null) found.clipEnd = 'Enter seconds or h:mm:ss.';
			else if (start !== null && end <= start) found.clipEnd = 'The end must come after the start.';
		}
		const options: NonNullable<SubmitRequest['options']> = {
			clip: null,
			subtitles,
			subtitle_language: subtitleLanguage.trim() || null,
			audio_language: audioLanguage ?? 'en'
		};
		if (start !== null || end !== null) {
			options.clip = { start: toDuration(start ?? 0), end: end === null ? null : toDuration(end) };
		}
		if (
			options.clip ||
			options.subtitles !== 'keep' ||
			options.subtitle_language ||
			options.audio_language !== 'en'
		) {
			request.options = options;
		}

		errors = found;
		return Object.keys(found).length === 0 ? request : null;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		const request = build();
		if (!request) return;
		pending = true;
		try {
			const { id } = await jobs.submit(request);
			notify.success('Queued', request.url);
			open = false;
			reset();
			await goto(resolve('/(app)/jobs/[id]', { id }));
		} catch (error) {
			reportError(error, 'Could not queue the link');
		} finally {
			pending = false;
		}
	}

	const MODES: { value: SubtitleMode; label: string }[] = [
		{ value: 'keep', label: 'Keep as separate files' },
		{ value: 'burn', label: 'Burn into the video' },
		{ value: 'skip', label: 'Skip' }
	];
</script>

<Modal
	bind:open
	title="Submit a link"
	description="The result posts to the web app."
	busy={pending}
>
	<form id="submit-form" class="space-y-4" onsubmit={submit}>
		<Field label="Link" for="submit-url" error={errors.url} required>
			<input
				id="submit-url"
				class="input"
				type="url"
				placeholder="https://"
				bind:value={url}
				required
				autocomplete="off"
			/>
		</Field>

		<Collapsible
			open={advanced}
			onOpenChange={(details) => (advanced = details.open)}
			class="items-start gap-4"
		>
			<Collapsible.Trigger class="btn preset-tonal btn-sm">
				Limits and options
				<Collapsible.Indicator class="group">
					<ChevronDownIcon class="size-4 transition group-data-[state=open]:rotate-180" />
				</Collapsible.Indicator>
			</Collapsible.Trigger>
			<Collapsible.Content class="w-full space-y-4">
				<div class="grid gap-4 sm:grid-cols-3">
					<Field label="Max source size" for="submit-size" help="MB" error={errors.maxSourceMb}>
						<input id="submit-size" class="input" type="number" min="1" bind:value={maxSourceMb} />
					</Field>
					<Field
						label="Max duration"
						for="submit-duration"
						help="Seconds or h:mm:ss"
						error={errors.maxDuration}
					>
						<input id="submit-duration" class="input" type="text" bind:value={maxDuration} />
					</Field>
					<Field label="Max height" for="submit-height" help="Pixels" error={errors.maxHeight}>
						<input id="submit-height" class="input" type="number" min="1" bind:value={maxHeight} />
					</Field>
				</div>
				<div class="grid gap-4 sm:grid-cols-2">
					<Field
						label="Clip start"
						for="submit-start"
						help="Seconds or h:mm:ss"
						error={errors.clipStart}
					>
						<input id="submit-start" class="input" type="text" bind:value={clipStart} />
					</Field>
					<Field label="Clip end" for="submit-end" help="Seconds or h:mm:ss" error={errors.clipEnd}>
						<input id="submit-end" class="input" type="text" bind:value={clipEnd} />
					</Field>
				</div>
				<div class="grid gap-4 sm:grid-cols-2">
					<Field label="Subtitles" for="submit-subtitles">
						<select id="submit-subtitles" class="select" bind:value={subtitles}>
							{#each MODES as mode (mode.value)}
								<option value={mode.value}>{mode.label}</option>
							{/each}
						</select>
					</Field>
					<Field label="Subtitle language" for="submit-language" help="Such as en or ja">
						<input
							id="submit-language"
							class="input"
							type="text"
							bind:value={subtitleLanguage}
							placeholder="Any"
						/>
					</Field>
					<Field label="Audio language" for="submit-audio-language">
						<LanguageSelect id="submit-audio-language" bind:value={audioLanguage} />
					</Field>
				</div>
			</Collapsible.Content>
		</Collapsible>
	</form>

	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (open = false)}
			disabled={pending}
		>
			Cancel
		</button>
		<button
			type="submit"
			form="submit-form"
			class="btn preset-filled-primary-500"
			disabled={pending}
		>
			{#if pending}<Spinner />{/if}
			Queue
		</button>
	{/snippet}
</Modal>
