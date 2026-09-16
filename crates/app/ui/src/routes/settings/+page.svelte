<script lang="ts">
	import { invalidate } from '$app/navigation';
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
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SettingsSection from '$lib/components/SettingsSection.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { ALL_SECTIONS, SECTIONS, flatten, sectionOf, type SectionSpec } from '$lib/settings/schema';
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

	// Contents: every section, with the one being read marked, and the ones with edits
	// that are not saved.

	const unsaved = new SvelteSet<string>();
	let active = $state(ALL_SECTIONS[0]!.key);
	// A section jumped to from the contents is held as the one being read until the reader
	// scrolls on, so a short section at the end of the page is not passed over for the
	// one before it.
	let pinned: string | null = null;
	let contentsNav = $state<HTMLElement | undefined>();
	let mobileContents = $state<HTMLDetailsElement | undefined>();

	function sectionElement(key: string): HTMLElement | null {
		return document.getElementById(`section-${key}`);
	}

	/**
	 * The section being read: the one under a line near the top of the window, or the next
	 * one when the line falls in the gap beneath a section that has scrolled past.
	 */
	function locate() {
		if (pinned !== null) return;
		const line = Math.min(160, window.innerHeight / 4);
		let current = ALL_SECTIONS[0]!.key;
		for (let i = 0; i < ALL_SECTIONS.length; i++) {
			const element = sectionElement(ALL_SECTIONS[i]!.key);
			if (!element) continue;
			const box = element.getBoundingClientRect();
			if (box.top > line) break;
			const next = ALL_SECTIONS[i + 1];
			current = box.bottom < line && next ? next.key : ALL_SECTIONS[i]!.key;
		}
		const root = document.documentElement;
		const atEnd =
			root.scrollHeight > window.innerHeight &&
			window.innerHeight + window.scrollY >= root.scrollHeight - 2;
		active = atEnd ? ALL_SECTIONS[ALL_SECTIONS.length - 1]!.key : current;
	}

	function jump(event: MouseEvent, key: string) {
		const element = sectionElement(key);
		if (!element) return;
		event.preventDefault();
		if (mobileContents) mobileContents.open = false;
		pinned = key;
		active = key;
		const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
		element.scrollIntoView({ block: 'start', behavior: reduce ? 'auto' : 'smooth' });
		element.focus({ preventScroll: true });
	}

	function markDirty(key: string, dirty: boolean) {
		if (dirty) unsaved.add(key);
		else unsaved.delete(key);
	}

	onMount(() => {
		let frame = 0;
		const schedule = () => {
			if (frame !== 0) return;
			frame = requestAnimationFrame(() => {
				frame = 0;
				locate();
			});
		};
		// The reader taking the scroll into their own hands ends a jump's hold: the wheel,
		// a touch, a key, or the mouse anywhere but on the contents themselves.
		const release = (event: Event) => {
			if (pinned === null) return;
			if (
				event.type === 'pointerdown' &&
				event.target instanceof Node &&
				(contentsNav?.contains(event.target) || mobileContents?.contains(event.target))
			) {
				return;
			}
			pinned = null;
			schedule();
		};
		const observer = new ResizeObserver(schedule);
		observer.observe(document.body);
		window.addEventListener('scroll', schedule, { passive: true });
		window.addEventListener('resize', schedule);
		window.addEventListener('wheel', release, { passive: true });
		window.addEventListener('touchstart', release, { passive: true });
		window.addEventListener('keydown', release);
		window.addEventListener('pointerdown', release);
		schedule();
		return () => {
			cancelAnimationFrame(frame);
			observer.disconnect();
			window.removeEventListener('scroll', schedule);
			window.removeEventListener('resize', schedule);
			window.removeEventListener('wheel', release);
			window.removeEventListener('touchstart', release);
			window.removeEventListener('keydown', release);
			window.removeEventListener('pointerdown', release);
		};
	});

	// The entry for the section being read is kept in view when the contents overflow.
	$effect(() => {
		const key = active;
		const nav = contentsNav;
		if (!nav) return;
		const link = nav.querySelector<HTMLElement>(`a[data-key="${CSS.escape(key)}"]`);
		if (!link) return;
		const box = nav.getBoundingClientRect();
		const own = link.getBoundingClientRect();
		if (own.top >= box.top && own.bottom <= box.bottom) return;
		nav.scrollTop += own.top - box.top - (box.height - own.height) / 2;
	});

	function within(section: SectionSpec): boolean {
		return active !== section.key && active.startsWith(`${section.key}.`);
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

{#snippet entries(sections: SectionSpec[], depth: number)}
	<ul class={['toc-list', depth > 0 && 'nested']}>
		{#each sections as section (section.key)}
			<li>
				<a
					href={`#section-${section.key}`}
					class={['toc-link', active === section.key && 'active', within(section) && 'within']}
					aria-current={active === section.key ? 'location' : undefined}
					data-key={section.key}
					onclick={(event) => jump(event, section.key)}
				>
					{#if depth === 0}<Icon name={section.icon} size={14} />{/if}
					<span class="truncate">{section.title}</span>
					{#if unsaved.has(section.key)}<span class="dot" title="Unsaved changes"></span>{/if}
				</a>
				{#if section.sections?.length}{@render entries(section.sections, depth + 1)}{/if}
			</li>
		{/each}
	</ul>
{/snippet}

{#snippet contents()}
	{@render entries(SECTIONS, 0)}
	{#if unsaved.size}
		<p class="toc-note"><span class="dot"></span>{pluralize(unsaved.size, 'section')} with unsaved changes</p>
	{/if}
{/snippet}

<PageHeader title="Settings" description="Everything the server runs on. A change is stored, logged and applied the moment it is saved.">
	{#snippet actions()}
		<Button icon="download" loading={exporting} onclick={() => exportAs('toml')}>Export</Button>
		<Button icon="file-text" onclick={openImport}>Import</Button>
	{/snippet}
</PageHeader>

<div class="layout">
	<div class="stack-lg">
		<details class="contents-mobile card" bind:this={mobileContents}>
			<summary><Icon name="rules" size={15} />On this page</summary>
			<div class="contents-mobile-body">{@render contents()}</div>
		</details>

		<section class="card">
			<div class="card-body">
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
			</div>
		</section>

		{#each groups as group (group.top.key)}
			<section class="group" id={`group-${group.top.key}`}>
				{#each group.sections as spec (spec.key)}
					<SettingsSection {spec} {view} onsave={save} {highlight} ondirty={(dirty) => markDirty(spec.key, dirty)} />
				{/each}
			</section>
		{/each}
	</div>

	<nav class="contents" aria-label="On this page" bind:this={contentsNav}>
		<span class="contents-title">On this page</span>
		{@render contents()}
	</nav>
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
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 224px;
		gap: 28px;
		align-items: start;
	}

	.group {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}

	/* The contents: beside the sections on a wide window, folded above them on a narrow one. */

	.contents {
		position: sticky;
		top: 24px;
		max-height: calc(100vh - 48px);
		overflow-y: auto;
		overscroll-behavior: contain;
		padding: 4px 0 8px;
		font-size: 13px;
		scrollbar-width: thin;
	}

	.contents-title {
		display: block;
		padding: 0 10px 8px;
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--text-3);
	}

	.toc-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}

	.toc-list.nested {
		margin: 1px 0 3px 17px;
		padding-left: 10px;
		border-left: 1px solid var(--border);
	}

	.toc-link {
		display: flex;
		align-items: center;
		gap: 8px;
		min-width: 0;
		padding: 5px 10px;
		border-radius: var(--radius-sm);
		color: var(--text-2);
		font-weight: 500;
		line-height: 1.3;
		transition:
			background-color 0.12s,
			color 0.12s;
	}

	.toc-link:hover {
		background: var(--surface-3);
		color: var(--text);
		text-decoration: none;
	}

	.toc-link.within {
		color: var(--text);
	}

	.toc-link.active {
		background: var(--accent-soft);
		color: var(--accent-text);
	}

	.dot {
		flex: none;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--warn);
	}

	.toc-link .dot {
		margin-left: auto;
	}

	.toc-note {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 10px 0 0;
		padding: 0 10px;
		font-size: 12px;
		color: var(--text-3);
	}

	.contents-mobile {
		display: none;
		padding: 0;
	}

	.contents-mobile summary {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 12px 16px;
		font-weight: 500;
		cursor: pointer;
		list-style: none;
	}

	.contents-mobile summary::-webkit-details-marker {
		display: none;
	}

	.contents-mobile summary::after {
		content: '';
		width: 7px;
		height: 7px;
		margin-left: auto;
		border-right: 1.5px solid var(--text-3);
		border-bottom: 1.5px solid var(--text-3);
		transform: rotate(45deg);
		transition: transform 0.12s;
	}

	.contents-mobile[open] summary::after {
		transform: rotate(-135deg);
	}

	.contents-mobile-body {
		padding: 4px 8px 12px;
		border-top: 1px solid var(--border);
		font-size: 13px;
	}

	@media (max-width: 1180px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}

		.contents {
			display: none;
		}

		.contents-mobile {
			display: block;
		}
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
