<script lang="ts">
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import DownloadIcon from '@lucide/svelte/icons/download';
	import UploadIcon from '@lucide/svelte/icons/upload';
	import { Accordion, FileUpload, Menu, Portal } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { settings as settingsApi } from '$lib/api/endpoints';
	import type { Json, SettingsFormat, SettingsView } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import SettingFieldRow from '$lib/components/SettingField.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import { number } from '$lib/format';
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
				'How many jobs run at once, the limits on what is taken, playlists, live capture, downloads, the archive, how long finished jobs are kept, which ffmpeg build and video encoders run, and how long jobs get to finish at shutdown.'
		},
		http: {
			title: 'Outgoing HTTP',
			description:
				'How the app reaches the platforms: the browser name it sends, timeouts, retries, how fast it may ask one host, and proxies.'
		},
		local: {
			title: 'Web submissions',
			description:
				'Where links submitted from the web app end up, the largest file kept there, and what the media is made to.'
		},
		discord: {
			title: 'Discord',
			description:
				'Upload limits by server boost level, what media posted to Discord is made to, and servers with a limit or target of their own, by server id.'
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
		backup: {
			title: 'Backups',
			description:
				'Whether the database is backed up, where, how often, and how many backups are kept. Backups are listed and made under Backups.'
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
	let importFileName = $state('');
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

	/** The sections that are folded shut. Every section starts open and stays open through filters. */
	let closed = $state<string[]>([]);
	const openSections = $derived(
		sections.map((section) => section.name).filter((name) => !closed.includes(name))
	);

	function onAccordionChange(value: string[]) {
		closed = sections.map((section) => section.name).filter((name) => !value.includes(name));
	}

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

	async function pickFile(file: File | undefined) {
		if (!file) return;
		importFileName = file.name;
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
			importFileName = '';
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

<PageHeader
	title="Settings"
	description="Changes apply as soon as they are saved. Values saved here win over the config file and the environment."
>
	{#snippet actions()}
		<SearchInput
			bind:value={filter}
			placeholder="Find a setting, such as engine.workers"
			debounce={0}
			class="w-72"
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
				<Menu.Positioner class="z-40">
					<Menu.Content>
						<Menu.ItemGroup>
							<Menu.ItemGroupLabel class="text-warning-600-400">
								Exports include secrets in plain text.
							</Menu.ItemGroupLabel>
							{#each FORMATS as format (format)}
								<Menu.Item value={format}>
									<Menu.ItemText>{FORMAT_LABELS[format]}</Menu.ItemText>
								</Menu.Item>
							{/each}
						</Menu.ItemGroup>
					</Menu.Content>
				</Menu.Positioner>
			</Portal>
		</Menu>
	{/snippet}
</PageHeader>

{#if view}
	<Card label="Where the settings live">
		<KeyValue class="sm:grid-cols-[auto_minmax(0,1fr)_auto_minmax(0,1fr)]">
			<KeyValueRow label="Data directory" value={view.data_dir} mono />
			<KeyValueRow label="Config file" value={view.provisioning_file} mono />
			<KeyValueRow label="Overridden" value={overridden} />
		</KeyValue>
	</Card>
{/if}

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
	<Card flush label="Settings by section">
		<!-- Skeleton's Accordion: one item per section, all open until folded by hand. -->
		<Accordion
			multiple
			collapsible
			value={openSections}
			onValueChange={(details) => onAccordionChange(details.value)}
			class="gap-0"
		>
			{#each sections as section, i (section.name)}
				{#if i > 0}
					<hr class="hr" />
				{/if}
				<Accordion.Item value={section.name}>
					<h3>
						<Accordion.ItemTrigger class="flex items-center justify-between gap-3">
							<span class="flex min-w-0 flex-wrap items-center gap-2">
								<span class="h6">{section.title}</span>
								<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
									{number(section.fields.length)}
								</span>
							</span>
							<Accordion.ItemIndicator class="group shrink-0">
								<ChevronDownIcon class="size-5 transition group-data-[state=open]:rotate-180" />
							</Accordion.ItemIndicator>
						</Accordion.ItemTrigger>
					</h3>
					<Accordion.ItemContent>
						{#if section.description}
							<p class="mb-2 text-sm text-surface-600-400">{section.description}</p>
						{/if}
						<div class="divide-y divide-surface-200-800">
							{#each section.fields as field (field.key)}
								<SettingFieldRow {field} onsave={save} onreset={reset} />
							{/each}
						</div>
					</Accordion.ItemContent>
				</Accordion.Item>
			{/each}
		</Accordion>
	</Card>
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
			<div class="label">
				<span class="label-text">File</span>
				<div class="flex flex-wrap items-center gap-3">
					<FileUpload
						class="w-fit"
						accept={{
							'application/toml': ['.toml'],
							'application/yaml': ['.yaml', '.yml'],
							'application/json': ['.json'],
							'text/plain': ['.toml', '.yaml', '.yml', '.json', '.txt']
						}}
						maxFiles={1}
						onFileAccept={(details) => void pickFile(details.files[0])}
					>
						<FileUpload.Trigger class="btn preset-tonal">
							<UploadIcon class="size-4" />
							Choose a file
						</FileUpload.Trigger>
						<FileUpload.HiddenInput />
					</FileUpload>
					{#if importFileName}
						<span class="text-sm text-surface-600-400">{importFileName}</span>
					{/if}
				</div>
			</div>
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
