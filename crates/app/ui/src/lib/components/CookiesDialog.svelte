<script lang="ts">
	import UploadIcon from '@lucide/svelte/icons/upload';
	import { FileUpload, SegmentedControl } from '@skeletonlabs/skeleton-svelte';
	import { platforms as platformsApi } from '$lib/api/endpoints';
	import type { CookieFormat, PlatformCoverage, SessionOutcome } from '$lib/api/types';
	import { notify, reportError } from '$lib/toast.svelte';
	import Field from './Field.svelte';
	import Modal from './Modal.svelte';
	import Spinner from './Spinner.svelte';

	interface Props {
		open?: boolean;
		platform: PlatformCoverage;
		onimported: (outcome: SessionOutcome) => void;
	}

	let { open = $bindable(false), platform, onimported }: Props = $props();

	let format = $state<CookieFormat>('netscape');
	let text = $state('');
	let domain = $state('');
	let pending = $state(false);
	let fileName = $state('');

	$effect(() => {
		if (open) {
			format = 'netscape';
			text = '';
			domain = platform.hosts[0] ?? '';
			fileName = '';
		}
	});

	async function pickFile(file: File | undefined) {
		if (!file) return;
		fileName = file.name;
		text = await file.text();
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		pending = true;
		try {
			const outcome = await platformsApi.setCookies(platform.id, {
				format,
				text,
				domain: format === 'header' ? domain.trim() || undefined : undefined
			});
			if (outcome.check_error) {
				notify.error('Cookies saved, the login could not be verified', outcome.check_error);
			} else if (outcome.session_check?.state === 'logged_in') {
				notify.success('Cookies saved', `Logged in as ${outcome.session_check.account}`);
			} else if (outcome.session_check?.state === 'logged_out') {
				notify.error('Cookies saved, but the session is logged out');
			} else {
				notify.success('Cookies saved');
			}
			open = false;
			onimported(outcome);
		} catch (err) {
			reportError(err, 'Could not import the cookies');
		} finally {
			pending = false;
		}
	}
</script>

<Modal
	bind:open
	title="Import cookies for {platform.name}"
	description="Cookies are encrypted at rest and never shown again. They replace any saved before."
	busy={pending}
	size="lg"
>
	<form id="cookies-form" class="space-y-4" onsubmit={submit}>
		<SegmentedControl
			value={format}
			onValueChange={(details) => {
				if (details.value === 'netscape' || details.value === 'header') format = details.value;
			}}
		>
			<SegmentedControl.Label>Format</SegmentedControl.Label>
			<SegmentedControl.Control>
				<SegmentedControl.Indicator />
				<SegmentedControl.Item value="netscape">
					<SegmentedControl.ItemText>Netscape cookies.txt</SegmentedControl.ItemText>
					<SegmentedControl.ItemHiddenInput />
				</SegmentedControl.Item>
				<SegmentedControl.Item value="header">
					<SegmentedControl.ItemText>Cookie header</SegmentedControl.ItemText>
					<SegmentedControl.ItemHiddenInput />
				</SegmentedControl.Item>
			</SegmentedControl.Control>
		</SegmentedControl>

		{#if format === 'netscape'}
			<div class="label">
				<span class="label-text">File</span>
				<div class="flex flex-wrap items-center gap-3">
					<FileUpload
						class="w-fit"
						accept={{ 'text/plain': ['.txt'] }}
						maxFiles={1}
						onFileAccept={(details) => void pickFile(details.files[0])}
					>
						<FileUpload.Trigger class="btn preset-tonal">
							<UploadIcon class="size-4" />
							Choose a file
						</FileUpload.Trigger>
						<FileUpload.HiddenInput />
					</FileUpload>
					{#if fileName}<span class="text-sm text-surface-600-400">{fileName}</span>{/if}
				</div>
				<p class="text-xs text-surface-600-400">
					Exported by a browser extension such as Get cookies.txt.
				</p>
			</div>
			<Field label="Or paste the file" for="cookies-text">
				<textarea
					id="cookies-text"
					class="textarea font-mono text-xs"
					rows="8"
					bind:value={text}
					spellcheck="false"
					placeholder="# Netscape HTTP Cookie File"></textarea>
			</Field>
		{:else}
			<Field label="Domain" for="cookies-domain" help="The host the cookies belong to." required>
				<input
					id="cookies-domain"
					class="input font-mono"
					type="text"
					bind:value={domain}
					required
				/>
			</Field>
			<Field
				label="Cookie header"
				for="cookies-header"
				help="The value of the Cookie request header, as the browser sends it."
				required
			>
				<textarea
					id="cookies-header"
					class="textarea font-mono text-xs"
					rows="5"
					bind:value={text}
					spellcheck="false"
					placeholder="name=value; other=value"></textarea>
			</Field>
		{/if}
	</form>
	{#snippet footer()}
		<button type="button" class="btn preset-tonal" onclick={() => (open = false)} disabled={pending}
			>Cancel</button
		>
		<button
			type="submit"
			form="cookies-form"
			class="btn preset-filled-primary-500"
			disabled={pending || !text.trim()}
			aria-busy={pending}
		>
			{#if pending}<Spinner />{/if}
			{pending ? 'Importing…' : 'Import and verify'}
		</button>
	{/snippet}
</Modal>
