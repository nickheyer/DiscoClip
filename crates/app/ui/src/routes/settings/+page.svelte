<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { onMount } from 'svelte';
	import type { PageData } from './$types';
	import { SETTINGS_FORMATS, messageOf, settings as api } from '$lib/api';
	import type { SettingsChange, SettingsFormat } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SettingsSection from '$lib/components/SettingsSection.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { SECTIONS, flatten, sectionOf } from '$lib/settings/schema';
	import { confirm } from '$lib/state/confirm.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const view = $derived(data.view);
	const highlight = $derived(data.key);
	const stored = $derived(view.entries.length);
	const provisioned = $derived(view.entries.filter((e) => e.source === 'provisioning').length);
	const latest = $derived(
		view.entries.reduce<string | null>(
			(best, entry) => (best === null || entry.updated_at > best ? entry.updated_at : best),
			null
		)
	);

	async function save(change: SettingsChange) {
		const changed = Object.keys(change.set ?? {}).length + (change.reset?.length ?? 0);
		await api.change(change);
		await invalidate('app:settings');
		toast.ok(`Saved ${pluralize(changed, 'setting')}. The server runs on them now.`);
	}

	onMount(() => {
		if (!data.key) return;
		const section = sectionOf(data.key);
		const target =
			document.getElementById(`setting-${data.key}`) ??
			(section ? document.getElementById(`section-${section.key}`) : null);
		target?.scrollIntoView({ block: 'center' });
	});

	// Export

	let exporting = $state(false);
	let exported = $state<{ format: SettingsFormat; text: string } | null>(null);

	async function exportAs(format: SettingsFormat) {
		exporting = true;
		try {
			exported = { format, text: await api.exportText(format) };
		} catch (cause) {
			toast.error(`Could not export the settings: ${messageOf(cause)}`);
		} finally {
			exporting = false;
		}
	}

	// Import

	let importDialog = $state(false);
	let importFormat = $state<SettingsFormat>('toml');
	let importText = $state('');
	let importing = $state(false);
	let importError = $state<string | null>(null);

	function openImport() {
		importFormat = 'toml';
		importText = '';
		importError = null;
		importDialog = true;
	}

	async function readFile(event: Event) {
		const input = event.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		if (!file) return;
		const name = file.name.toLowerCase();
		if (name.endsWith('.yaml') || name.endsWith('.yml')) importFormat = 'yaml';
		else if (name.endsWith('.json')) importFormat = 'json';
		else importFormat = 'toml';
		importText = await file.text();
		input.value = '';
	}

	async function runImport(event: SubmitEvent) {
		event.preventDefault();
		if (importText.trim() === '') return;
		const ok = await confirm.ask({
			title: 'Import these settings?',
			message:
				'Every key in the file is written over what is stored, as if set here, and takes effect at once.',
			confirmLabel: 'Import'
		});
		if (!ok) return;
		importing = true;
		importError = null;
		try {
			const before = view.entries.length;
			const after = await api.import({ format: importFormat, text: importText });
			importDialog = false;
			await invalidate('app:settings');
			toast.ok(
				`Imported. ${pluralize(after.entries.length, 'setting')} stored${after.entries.length !== before ? ` (was ${before})` : ''}.`
			);
		} catch (cause) {
			importError = messageOf(cause);
		} finally {
			importing = false;
		}
	}

	const groups = $derived(SECTIONS.map((top) => ({ top, sections: flatten([top]) })));
</script>

<svelte:head>
	<title>Settings · DiscoClip</title>
</svelte:head>

<PageHeader title="Settings" description="Everything the server runs on. A change is stored, logged and applied the moment it is saved.">
	{#snippet actions()}
		<Button icon="download" loading={exporting} onclick={() => exportAs('toml')}>Export</Button>
		<Button icon="file-text" onclick={openImport}>Import</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-body overview">
			<dl class="kv">
				<dt>Stored</dt>
				<dd>
					{pluralize(stored, 'value')} over the defaults
					{#if provisioned}<span class="faint">· {provisioned} provisioned</span>{/if}
					{#if latest}<span class="faint">· last change <Time value={latest} /></span>{/if}
				</dd>
				<dt>Data directory</dt>
				<dd class="row"><code>{view.data_dir}</code><span class="faint small">Holds the database and the secret key; set by the provisioning file or <code>DISCOCLIP_DATA_DIR</code> only.</span></dd>
				<dt>Provisioning file</dt>
				<dd>
					{#if view.provisioning_file}
						<code>{view.provisioning_file}</code>
						<span class="faint small">Read at startup. A value changed here is kept even when the file still names the old one.</span>
					{:else}
						<span class="faint">None was found at startup; the environment alone provisions.</span>
					{/if}
				</dd>
			</dl>
			<nav class="jump" aria-label="Sections">
				{#each SECTIONS as top (top.key)}
					<a href={`#section-${top.key}`}><Icon name={top.icon} size={14} />{top.title}</a>
				{/each}
			</nav>
		</div>
	</section>

	{#each groups as group (group.top.key)}
		<section class="group" id={`group-${group.top.key}`}>
			{#each group.sections as spec (spec.key)}
				<SettingsSection {spec} {view} onsave={save} {highlight} />
			{/each}
		</section>
	{/each}
</div>

{#if exported}
	<Dialog open title="Exported settings" description="The stored values as a provisioning file. Secrets are included in the clear." size="lg" onclose={() => (exported = null)}>
		<div class="stack">
			<div class="row">
				{#each SETTINGS_FORMATS as format (format)}
					<Button size="sm" variant={exported.format === format ? 'primary' : 'secondary'} onclick={() => exportAs(format)} loading={exporting && exported.format !== format}>{format.toUpperCase()}</Button>
				{/each}
				<span class="grow"></span>
				<CopyButton text={exported.text} label="Copy" />
				<Button size="sm" icon="download" href={api.exportUrl(exported.format)} external>Download</Button>
			</div>
			<pre class="export">{exported.text || '# Nothing is stored; every setting is at its default.'}</pre>
		</div>
		{#snippet footer()}
			<Button variant="primary" onclick={() => (exported = null)}>Close</Button>
		{/snippet}
	</Dialog>
{/if}

<Dialog bind:open={importDialog} title="Import settings" description="A provisioning file, as discoclip.toml, .yaml or .json. Every key it names is stored as if set here." size="lg" busy={importing}>
	<form id="import-form" class="stack" onsubmit={runImport} novalidate>
		{#if importError}
			<Alert tone="danger" message={importError} onclose={() => (importError = null)} />
		{/if}
		<div class="grid-2">
			<Field label="Format" for="import-format">
				<select id="import-format" class="select" bind:value={importFormat}>
					{#each SETTINGS_FORMATS as format (format)}
						<option value={format}>{format.toUpperCase()}</option>
					{/each}
				</select>
			</Field>
			<Field label="File" for="import-file" hint="Or paste the text below.">
				<input id="import-file" class="input" type="file" accept=".toml,.yaml,.yml,.json" onchange={readFile} />
			</Field>
		</div>
		<Field label="Text" for="import-text">
			<textarea id="import-text" class="textarea mono" rows="14" bind:value={importText} spellcheck="false"></textarea>
		</Field>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (importDialog = false)} disabled={importing}>Cancel</Button>
		<Button variant="primary" loading={importing} disabled={importText.trim() === ''} onclick={() => document.querySelector<HTMLFormElement>('#import-form')?.requestSubmit()}>Import</Button>
	{/snippet}
</Dialog>

<style>
	.overview {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}

	.jump {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}

	.jump a {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px;
		border-radius: 999px;
		background: var(--surface-3);
		color: var(--text-2);
		font-size: 12.5px;
		font-weight: 500;
	}

	.jump a:hover {
		background: var(--accent-soft);
		color: var(--accent-text);
		text-decoration: none;
	}

	.group {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}

	.grow {
		flex: 1;
	}

	.export {
		padding: 12px;
		border-radius: var(--radius-sm);
		background: var(--surface-2);
		border: 1px solid var(--border);
		max-height: 420px;
		overflow: auto;
	}
</style>
