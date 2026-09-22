<script lang="ts">
	import FormFeedback from '$lib/components/FormFeedback.svelte';
	import { invalidate, replaceState } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import type { PageData } from './$types';
	import { SETTINGS_FORMATS, messageOf, settings as api } from '$lib/api';
	import type { SettingsChange, SettingsFormat } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SettingsSection from '$lib/components/SettingsSection.svelte';
	import { pluralize } from '$lib/format';
	import { ALL_SECTIONS, SECTIONS, flatten, sectionOf } from '$lib/settings/schema';
	import { confirm } from '$lib/state/confirm.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();
	const view = $derived(data.view);
	const highlight = $derived(data.key);
	const unsaved = new SvelteSet<string>();
	let active = $state('engine');
	let search = $state('');
	const groups = SECTIONS.map((top) => ({ top, sections: flatten([top]) }));
	const matches = $derived(ALL_SECTIONS.filter((section) => {
		const text = [section.title, section.key, section.description,
			...section.fields.map((field) => `${field.label} ${field.name} ${field.hint}`),
			...(section.maps ?? []).map((map) => `${map.label} ${map.key} ${map.hint}`)
		].join(' ').toLowerCase();
		return search.trim().toLowerCase().split(/\s+/).every((word) => text.includes(word));
	}));

	function select(key: string) {
		active = key;
		search = '';
		if (typeof window !== 'undefined') replaceState(`#section-${key}`, page.state);
	}

	$effect(() => {
		if (data.key) active = sectionOf(data.key)?.key ?? 'engine';
	});

	onMount(() => {
		function fromHash() {
			const key = location.hash.replace('#section-', '');
			if (ALL_SECTIONS.some((section) => section.key === key)) active = key;
		}
		fromHash();
		window.addEventListener('hashchange', fromHash);
		return () => window.removeEventListener('hashchange', fromHash);
	});

	function markDirty(key: string, dirty: boolean) {
		if (dirty) unsaved.add(key);
		else unsaved.delete(key);
	}

	async function save(change: SettingsChange) {
		await api.change(change);
		await invalidate('app:settings');
		toast.ok('Settings saved.');
	}

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
				'Settings in this file replace the saved values and apply immediately.',
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

</script>

<svelte:head><title>Settings · DiscoClip</title></svelte:head>

<PageHeader title="Settings">
	{#snippet actions()}
		<Button icon="download" loading={exporting} onclick={() => exportAs('toml')}>Export</Button>
		<Button icon="file-text" onclick={openImport}>Import</Button>
	{/snippet}
</PageHeader>

<div class="settings-layout">
	<aside class="settings-nav">
		<Field label="Find a setting" for="settings-search">
			<input id="settings-search" class="input" type="search" bind:value={search} placeholder="Search settings" />
		</Field>
		<div class="mobile-select">
			<Field label="Section" for="settings-section">
				<select id="settings-section" class="select" value={active} onchange={(event) => select(event.currentTarget.value)}>
					{#each groups as group (group.top.key)}
						<optgroup label={group.top.title}>
							{#each group.sections as spec (spec.key)}<option value={spec.key}>{spec.title}{unsaved.has(spec.key) ? ' (unsaved)' : ''}</option>{/each}
						</optgroup>
					{/each}
				</select>
			</Field>
		</div>
		<nav aria-label="Settings sections">
			{#each groups as group (group.top.key)}
				{#if group.sections.some((spec) => matches.includes(spec))}
					<div class="nav-group">
						<h2>{group.top.title}</h2>
						{#each group.sections as spec (spec.key)}
							{#if matches.includes(spec)}
								<a href={`#section-${spec.key}`} class:active={active === spec.key && !search.trim()} aria-current={active === spec.key && !search.trim() ? 'location' : undefined} onclick={(event) => { event.preventDefault(); select(spec.key); }}>
									<span>{spec.key === group.top.key ? 'General' : spec.title}</span>
									{#if unsaved.has(spec.key)}<span class="unsaved" aria-label="Unsaved changes">•</span>{/if}
								</a>
							{/if}
						{/each}
					</div>
				{/if}
			{/each}
		</nav>
		{#if unsaved.size}<p class="hint" role="status">{pluralize(unsaved.size, 'section')} with unsaved changes</p>{/if}
	</aside>

	<div class="settings-content stack">
		{#if search.trim()}
			<div class="row-between" role="status">
				<p>{pluralize(matches.length, 'matching section')}</p>
				<Button variant="ghost" onclick={() => (search = '')}>Clear search</Button>
			</div>
		{/if}
		{#each ALL_SECTIONS as spec (spec.key)}
			<div hidden={search.trim() ? !matches.includes(spec) : active !== spec.key}>
				<SettingsSection {spec} {view} onsave={save} {highlight} ondirty={(dirty) => markDirty(spec.key, dirty)} />
			</div>
		{/each}
		<details class="server-details">
			<summary>Server details</summary>
			<dl class="kv">
				<dt>Data directory</dt><dd><code>{view.data_dir}</code></dd>
				<dt>Config file</dt><dd>{view.provisioning_file ?? 'None'}</dd>
				<dt>Saved settings</dt><dd>{view.entries.length}</dd>
			</dl>
		</details>
	</div>
</div>

{#if exported}
	<Dialog open title="Exported settings" description="Includes passwords and secrets in plain text." size="lg" onclose={() => (exported = null)}>
		<div class="stack">
			<div class="row">
				{#each SETTINGS_FORMATS as format (format)}
					<Button size="sm" variant={exported.format === format ? 'primary' : 'secondary'} onclick={() => exportAs(format)} loading={exporting && exported.format !== format}>{format.toUpperCase()}</Button>
				{/each}
				<span class="grow"></span>
				<CopyButton text={exported.text} label="Copy" />
				<Button size="sm" icon="download" href={api.exportUrl(exported.format)} external>Download</Button>
			</div>
			<pre class="export">{exported.text || '# All settings use defaults.'}</pre>
		</div>
		{#snippet footer()}
			<Button variant="primary" onclick={() => (exported = null)}>Close</Button>
		{/snippet}
	</Dialog>
{/if}

<Dialog bind:open={importDialog} title="Import settings" description="Upload a TOML, YAML or JSON file, or paste its contents." size="lg" busy={importing}>
	<form id="import-form" class="stack" onsubmit={runImport} novalidate>
		{#if importError}
			<FormFeedback message={importError} />
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
	.settings-layout { display: grid; grid-template-columns: 220px minmax(0, 1fr); gap: 32px; align-items: start; }
	.settings-nav { position: sticky; top: 24px; display: flex; flex-direction: column; gap: 20px; max-height: calc(100dvh - 48px); overflow-y: auto; padding: 2px 8px 2px 2px; }
	.settings-nav nav { display: flex; flex-direction: column; gap: 20px; }
	.nav-group h2 { font-size: 13px; padding: 0 12px 6px; color: var(--text-2); }
	.nav-group a { display: flex; align-items: center; justify-content: space-between; min-height: 40px; padding: 8px 12px; color: var(--text-2); border-radius: var(--radius-sm); }
	.nav-group a:hover { background: var(--surface-3); text-decoration: none; }
	.nav-group a.active { color: var(--accent-text); background: var(--accent-soft); font-weight: 600; }
	.unsaved { color: var(--warn-text); }
	.mobile-select { display: none; }
	.settings-content { min-width: 0; }
	.server-details { color: var(--text-2); }
	.server-details summary { font-weight: 500; }
	.server-details dl { padding-block: 12px; }
	.grow { flex: 1; }
	.export { padding: 16px; background: var(--surface-2); border: 1px solid var(--border); border-radius: var(--radius-sm); max-height: 420px; overflow: auto; }
	@media (max-width: 1100px) {
		.settings-layout { grid-template-columns: minmax(0, 1fr); gap: 24px; }
		.settings-nav { position: static; max-height: none; overflow: visible; padding: 0; }
		.settings-nav nav { display: none; }
		.mobile-select { display: block; }
	}
</style>
