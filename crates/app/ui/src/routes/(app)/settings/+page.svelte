<script lang="ts">
	import DownloadIcon from '@lucide/svelte/icons/download';
	import UploadIcon from '@lucide/svelte/icons/upload';
	import { Menu, Portal } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { settings as settingsApi } from '$lib/api/endpoints';
	import type { Json, SettingsFormat, SettingsView } from '$lib/api/types';
	import Confirm from '$lib/components/Confirm.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import SettingFieldRow from '$lib/components/SettingField.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { EMPTY, number } from '$lib/format';
	import { fieldsOf } from '$lib/settings';
	import { notify, reportError } from '$lib/toast.svelte';

	const FORMAT_LABELS: Record<SettingsFormat, string> = {
		toml: 'TOML',
		yaml: 'YAML',
		json: 'JSON'
	};
	const FORMATS = Object.keys(FORMAT_LABELS) as SettingsFormat[];

	const SECTIONS: Record<string, { title: string; description: string }> = {
		log: {
			title: 'Logging',
			description:
				'The log level, such as info or debug. Filter directives like discoclip=debug,hyper=warn also work.'
		},
		engine: {
			title: 'Engine',
			description:
				'How many jobs run at once, the limits on what is taken, playlists, live capture, downloads, the archive and how long finished jobs are kept.'
		},
		http: {
			title: 'Outgoing HTTP',
			description:
				'How the app reaches the platforms: the browser name it sends, timeouts, retries, how fast it may ask one host, and proxies.'
		},
		local: {
			title: 'Web submissions',
			description: 'Where links submitted from the web app end up, and the largest file kept there.'
		},
		fixtures: {
			title: 'Platform checks',
			description: 'How often the check links run and how long each may take.'
		},
		web: {
			title: 'Web app',
			description:
				'The address the app listens on, the address browsers reach it at, the proxies in front of it and its HTTPS certificate.'
		},
		auth: {
			title: 'Login providers',
			description:
				'GitHub, Google and OpenID Connect. Register the callback https://<public_url>/api/auth/<provider>/callback at the provider. Discord login is set up per application.'
		}
	};

	let view = $state<SettingsView | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let filter = $state('');

	let importOpen = $state(false);
	let importFormat = $state<SettingsFormat>('toml');
	let importText = $state('');
	let importing = $state(false);

	let exportOpen = $state(false);
	let exportFormat = $state<SettingsFormat>('toml');

	async function load() {
		loading = true;
		error = null;
		try {
			view = await settingsApi.get();
		} catch (err) {
			error = err;
		} finally {
			loading = false;
		}
	}

	onMount(() => void load());

	const fields = $derived(view ? fieldsOf(view) : []);
	const shown = $derived.by(() => {
		const needle = filter.trim().toLowerCase();
		return needle ? fields.filter((f) => f.key.toLowerCase().includes(needle)) : fields;
	});
	const sections = $derived.by(() => {
		const order = Object.keys(SECTIONS);
		const names = [...new Set(shown.map((f) => f.section))].sort(
			(a, b) =>
				(order.indexOf(a) === -1 ? 99 : order.indexOf(a)) -
				(order.indexOf(b) === -1 ? 99 : order.indexOf(b))
		);
		return names.map((name) => ({
			name,
			title: SECTIONS[name]?.title ?? name,
			description: SECTIONS[name]?.description ?? '',
			fields: shown.filter((f) => f.section === name)
		}));
	});

	/** What the store holds over the defaults, by who wrote it. */
	const overridden = $derived.by(() => {
		if (!view) return '';
		const app = view.entries.filter((entry) => entry.source === 'app').length;
		const provisioned = view.entries.length - app;
		if (app === 0 && provisioned === 0) return 'Nothing overridden';
		const values = app === 1 ? 'value' : 'values';
		return `${number(app)} ${values} saved in the app · ${number(provisioned)} from the config file`;
	});

	const exportLabel = $derived(FORMAT_LABELS[exportFormat]);

	async function save(key: string, value: Json) {
		try {
			view = await settingsApi.set(key, value);
			notify.success('Setting applied', key);
		} catch (err) {
			reportError(err, `Could not save ${key}`);
			throw err;
		}
	}

	async function reset(key: string) {
		try {
			view = await settingsApi.reset(key);
			notify.success('Setting reset', key);
		} catch (err) {
			reportError(err, `Could not reset ${key}`);
			throw err;
		}
	}

	async function pickFile(event: Event) {
		const input = event.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		if (!file) return;
		importText = await file.text();
		const ext = file.name.split('.').pop()?.toLowerCase();
		if (ext === 'yaml' || ext === 'yml') importFormat = 'yaml';
		else if (ext === 'json') importFormat = 'json';
		else if (ext === 'toml') importFormat = 'toml';
	}

	async function runImport(event: SubmitEvent) {
		event.preventDefault();
		importing = true;
		try {
			view = await settingsApi.import({ format: importFormat, text: importText });
			notify.success('Settings imported and applied');
			importOpen = false;
			importText = '';
		} catch (err) {
			reportError(err, 'Could not import the settings');
		} finally {
			importing = false;
		}
	}

	function chooseExport(format: string) {
		if (format !== 'toml' && format !== 'yaml' && format !== 'json') return;
		exportFormat = format;
		exportOpen = true;
	}

	function runExport() {
		location.assign(settingsApi.exportUrl(exportFormat));
	}
</script>

<PageHeader title="Settings" />

{#if view}
	<div class="flex flex-wrap items-center gap-x-6 gap-y-2 card bg-surface-100-900 p-3 text-sm">
		<span
			><span class="text-surface-600-400">Data directory</span>
			<span class="font-mono">{view.data_dir}</span></span
		>
		<span
			><span class="text-surface-600-400">Config file</span>
			{#if view.provisioning_file}
				<span class="font-mono">{view.provisioning_file}</span>
			{:else}
				<span class="text-surface-600-400">{EMPTY}</span>
			{/if}</span
		>
		<span class="text-surface-600-400">{overridden}</span>
	</div>
{/if}

<Toolbar
	description="Changes apply as soon as they are saved. Values saved here win over the config file and the environment."
>
	<SearchInput
		bind:value={filter}
		placeholder="Find a setting, such as engine.workers"
		debounce={0}
		class="mr-auto w-full max-w-lg"
	/>
	<button type="button" class="btn preset-tonal" onclick={() => (importOpen = true)}>
		<UploadIcon class="size-4" />
		Import
	</button>
	<Menu onSelect={(details) => chooseExport(details.value)}>
		<Menu.Trigger class="btn preset-tonal">
			<DownloadIcon class="size-4" />
			Export
		</Menu.Trigger>
		<Portal>
			<Menu.Positioner>
				<Menu.Content class="max-w-72 min-w-56 card bg-surface-100-900 p-2 shadow-xl">
					<p class="px-2 pt-1 pb-2 text-sm text-warning-800-200">
						Exports include secrets in plain text.
					</p>
					{#each FORMATS as format (format)}
						<Menu.Item value={format}>
							<Menu.ItemText>{FORMAT_LABELS[format]}</Menu.ItemText>
						</Menu.Item>
					{/each}
				</Menu.Content>
			</Menu.Positioner>
		</Portal>
	</Menu>
</Toolbar>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if !view}
	<div class="space-y-4" aria-busy="true">
		{#each { length: 4 }, i (i)}
			<div class="h-32 placeholder animate-pulse"></div>
		{/each}
	</div>
{:else if sections.length === 0}
	<p class="text-sm text-surface-600-400">No setting matches.</p>
{:else}
	{#each sections as section (section.name)}
		<section
			class="card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-label={section.title}
		>
			<h2 class="h6">{section.title}</h2>
			{#if section.description}<p class="mb-2 text-sm text-surface-600-400">
					{section.description}
				</p>{/if}
			<div class="divide-y divide-surface-200-800">
				{#each section.fields as field (field.key)}
					<SettingFieldRow {field} onsave={save} onreset={reset} />
				{/each}
			</div>
		</section>
	{/each}
{/if}

<Confirm
	bind:open={exportOpen}
	title="Export settings as {exportLabel}?"
	message="The file includes every secret in plain text."
	confirmLabel="Export"
	onconfirm={runExport}
/>

<Modal
	bind:open={importOpen}
	title="Import settings"
	description="Values in the file are saved as app settings and applied."
	busy={importing}
	size="lg"
>
	<form id="settings-import" class="space-y-4" onsubmit={runImport}>
		<div class="grid gap-4 sm:grid-cols-2">
			<Field label="Format" for="import-format">
				<select id="import-format" class="select" bind:value={importFormat}>
					{#each FORMATS as format (format)}
						<option value={format}>{FORMAT_LABELS[format]}</option>
					{/each}
				</select>
			</Field>
			<Field label="File" for="import-file">
				<input
					id="import-file"
					class="input"
					type="file"
					accept=".toml,.yaml,.yml,.json,text/plain"
					onchange={pickFile}
				/>
			</Field>
		</div>
		<Field label="Or paste" for="import-text">
			<textarea
				id="import-text"
				class="textarea font-mono text-xs"
				rows="12"
				bind:value={importText}
				spellcheck="false"></textarea>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (importOpen = false)}
			disabled={importing}>Cancel</button
		>
		<button
			type="submit"
			form="settings-import"
			class="btn preset-filled-primary-500"
			disabled={importing || !importText.trim()}
		>
			{#if importing}<Spinner />{/if}
			Import and apply
		</button>
	{/snippet}
</Modal>
