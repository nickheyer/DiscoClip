<script lang="ts">
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

	async function pickFile(event: Event) {
		const input = event.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
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
		<fieldset class="flex gap-4">
			<legend class="mb-1 label-text">Format</legend>
			<label class="flex items-center gap-2 text-sm">
				<input
					class="radio"
					type="radio"
					name="cookie-format"
					value="netscape"
					bind:group={format}
				/>
				Netscape cookies.txt
			</label>
			<label class="flex items-center gap-2 text-sm">
				<input class="radio" type="radio" name="cookie-format" value="header" bind:group={format} />
				Cookie header
			</label>
		</fieldset>

		{#if format === 'netscape'}
			<Field
				label="File"
				for="cookies-file"
				help="Exported by a browser extension such as Get cookies.txt."
			>
				<input
					id="cookies-file"
					class="input"
					type="file"
					accept=".txt,text/plain"
					onchange={pickFile}
				/>
				{#if fileName}<p class="text-sm text-surface-600-400">{fileName}</p>{/if}
			</Field>
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
