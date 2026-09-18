<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { MEDIA_LABELS, PLATFORM_DEFAULTS, messageOf, profiles as api } from '$lib/api';
	import type { PlatformCoverage, PlatformDefault, Profile, ProfileInput } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { MEDIA_HINTS } from '$lib/platforms';
	import {
		DEFAULT_LABELS,
		emptyProfile,
		preview,
		profileName,
		tally,
		toggleOf,
		usesPresets,
		withPreset,
		withToggle
	} from '$lib/profiles';
	import type { Toggle } from '$lib/profiles';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const canEdit = $derived(session.can('manage_settings'));
	const platformIds = $derived(data.platforms.map((p) => p.id));
	const platformsById = $derived(new Map(data.platforms.map((p) => [p.id, p])));
	const sorted = $derived([...data.platforms].sort((a, b) => a.name.localeCompare(b.name)));
	const globalId = $derived(data.global?.profile_id ?? null);
	const refresh = () => invalidate('app:profiles');

	// The whole server's profile

	let globalChoice = $state('');
	let savingGlobal = $state(false);
	$effect(() => {
		globalChoice = globalId ?? '';
	});

	async function saveGlobal() {
		if (!globalChoice || globalChoice === globalId) return;
		savingGlobal = true;
		try {
			await api.assign({ kind: 'global' }, globalChoice);
			toast.ok(`${profileName(data.profiles, globalChoice)} is now in force for the whole server.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not change the server's profile: ${messageOf(cause)}`);
		} finally {
			savingGlobal = false;
		}
	}

	// Add, edit and view

	let dialog = $state(false);
	let editing = $state<Profile | null>(null);
	let readOnly = $state(false);
	let form = $state<ProfileInput>(emptyProfile());
	let saving = $state(false);
	let error = $state<string | null>(null);
	let search = $state('');

	function openAdd() {
		editing = null;
		readOnly = false;
		form = emptyProfile();
		error = null;
		search = '';
		dialog = true;
	}

	function openEdit(profile: Profile) {
		editing = profile;
		readOnly = !canEdit;
		form = {
			name: profile.name,
			description: profile.description,
			platforms: {
				default: profile.platforms.default,
				presets: [...profile.platforms.presets],
				overrides: { ...profile.platforms.overrides }
			}
		};
		error = null;
		search = '';
		dialog = true;
	}

	const nameProblem = $derived(form.name.trim() === '' ? 'A profile needs a name.' : null);
	const visible = $derived.by(() => {
		const needle = search.trim().toLowerCase();
		if (!needle) return sorted;
		return sorted.filter(
			(p) =>
				p.name.toLowerCase().includes(needle) ||
				p.id.includes(needle) ||
				p.hosts.some((h) => h.includes(needle)) ||
				p.media.some((m) => m.includes(needle))
		);
	});
	const formPreview = $derived(preview(form.platforms, platformIds, data.presets));
	const formTally = $derived(tally(form.platforms, platformIds, data.presets));
	const formUsesPresets = $derived(usesPresets(form.platforms));

	function setPreset(preset: string, chosen: boolean) {
		if (readOnly) return;
		form.platforms = withPreset(form.platforms, preset, chosen);
	}

	function presetLabel(id: string): string {
		return data.presets.find((p) => p.id === id)?.label ?? id;
	}

	function setToggle(platform: string, toggle: Toggle) {
		if (readOnly) return;
		form.platforms = withToggle(form.platforms, platform, toggle);
	}

	function setVisible(toggle: Toggle) {
		if (readOnly) return;
		let next = form.platforms;
		for (const platform of visible) next = withToggle(next, platform.id, toggle);
		form.platforms = next;
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (readOnly) {
			dialog = false;
			return;
		}
		if (nameProblem) {
			error = nameProblem;
			return;
		}
		saving = true;
		error = null;
		const input: ProfileInput = {
			name: form.name.trim(),
			description: form.description.trim(),
			platforms: form.platforms
		};
		try {
			if (editing) {
				await api.update(editing.id, input);
				toast.ok(`Profile ${input.name} saved.`);
			} else {
				await api.create(input);
				toast.ok(`Profile ${input.name} added.`);
			}
			dialog = false;
			await refresh();
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	let deleting = $state<string | null>(null);

	async function remove(profile: Profile) {
		const ok = await confirm.ask({
			title: `Remove the profile ${profile.name}?`,
			message:
				'Every guild, channel and user it is in force for goes back to what the wider scope allows.',
			confirmLabel: 'Remove profile',
			danger: true
		});
		if (!ok) return;
		deleting = profile.id;
		try {
			await api.remove(profile.id);
			toast.ok(`Profile ${profile.name} removed.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the profile: ${messageOf(cause)}`);
		} finally {
			deleting = null;
		}
	}

	function summary(profile: Profile): string {
		const t = tally(profile.platforms, platformIds, data.presets);
		const parts: string[] = [];
		if (t.on) parts.push(`${t.on} on`);
		if (t.off) parts.push(`${t.off} off`);
		if (t.inherit) parts.push(`${t.inherit} as the wider scope has them`);
		return parts.join(' · ');
	}

	function named(profile: Profile): string[] {
		return Object.keys(profile.platforms.overrides)
			.map((id) => platformsById.get(id)?.name ?? id)
			.sort((a, b) => a.localeCompare(b));
	}

	const TOGGLES: { value: Toggle; label: string }[] = [
		{ value: 'inherit', label: 'Default' },
		{ value: 'on', label: 'On' },
		{ value: 'off', label: 'Off' }
	];
</script>

<svelte:head>
	<title>Profiles · DiscoClip</title>
</svelte:head>

<PageHeader
	title="Profiles"
	description="Which platforms are on where. A profile names platforms to turn on or off; it is put in force for the whole server, a guild, a channel or a user, and the narrowest scope wins."
>
	{#snippet actions()}
		{#if canEdit}
			<Button variant="primary" icon="plus" onclick={openAdd}>New profile</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-header">
			<div>
				<h2>Whole server</h2>
				<p class="hint">In force wherever no guild, channel or user has a profile of its own. Links submitted from the web app follow it.</p>
			</div>
		</div>
		<div class="card-body">
			{#if canEdit}
				<div class="row">
					<select class="select global" bind:value={globalChoice} aria-label="Profile for the whole server" disabled={savingGlobal}>
						{#each data.profiles as profile (profile.id)}
							<option value={profile.id}>{profile.name}</option>
						{/each}
					</select>
					<Button variant="primary" loading={savingGlobal} disabled={!globalChoice || globalChoice === globalId} onclick={saveGlobal}>Put in force</Button>
					{#if globalId}
						<span class="faint small">now {profileName(data.profiles, globalId)}{#if data.global} since <Time value={data.global.updated_at} />{/if}</span>
					{/if}
				</div>
			{:else if globalId}
				<span class="row"><span class="strong">{profileName(data.profiles, globalId)}</span>{#if data.global}<span class="faint small">since <Time value={data.global.updated_at} /></span>{/if}</span>
			{/if}
		</div>
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Profiles</h2>
				<p class="hint">Guild managers pick from these for their guilds, channels and members.</p>
			</div>
			<span class="faint small">{pluralize(data.profiles.length, 'profile')}</span>
		</div>
		{#if data.profiles.length === 0}
			<div class="card-body"><Empty compact icon="sparkles" title="No profiles" description="The server has no profiles." /></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Profile</th>
							<th>Unnamed platforms</th>
							<th>Named platforms</th>
							<th>Amounts to</th>
							<th>Updated</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each data.profiles as profile (profile.id)}
							{@const names = named(profile)}
							<tr>
								<td>
									<div class="stack-sm" style="gap:2px">
										<span class="row">
											<span class="strong">{profile.name}</span>
											{#if profile.builtin}<Badge size="sm" tone="info">Built in</Badge>{/if}
											{#if profile.id === globalId}<Badge size="sm" tone="ok" dot>Whole server</Badge>{/if}
										</span>
										{#if profile.description}<span class="muted small">{profile.description}</span>{/if}
									</div>
								</td>
								<td>
									{#if usesPresets(profile.platforms)}
										<div class="stack-sm" style="gap:4px">
											<span class="small">Only the presets' platforms</span>
											<div class="chips">
												{#each profile.platforms.presets as preset (preset)}<span class="chip preset">{presetLabel(preset)}</span>{/each}
											</div>
										</div>
									{:else}
										{DEFAULT_LABELS[profile.platforms.default].label}
									{/if}
								</td>
								<td>
									{#if names.length === 0}
										<span class="faint">none</span>
									{:else}
										<div class="chips">
											{#each names.slice(0, 6) as name (name)}<span class="chip">{name}</span>{/each}
											{#if names.length > 6}<span class="chip">+{names.length - 6}</span>{/if}
										</div>
									{/if}
								</td>
								<td class="small">{summary(profile)}</td>
								<td><Time value={profile.updated_at} /></td>
								<td class="actions">
									{#if canEdit}
										<Button size="sm" variant="ghost" icon="pencil" onclick={() => openEdit(profile)}>Edit</Button>
										{#if !profile.builtin}
											<Button size="sm" variant="ghost" icon="trash" loading={deleting === profile.id} disabled={profile.id === globalId} title={profile.id === globalId ? 'In force for the whole server; put another in force first' : 'Remove profile'} onclick={() => remove(profile)} square />
										{/if}
									{:else}
										<Button size="sm" variant="ghost" icon="eye" onclick={() => openEdit(profile)}>View</Button>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>
</div>

<Dialog
	bind:open={dialog}
	title={readOnly ? `Profile ${editing?.name ?? ''}` : editing ? `Edit profile ${editing.name}` : 'New profile'}
	size="lg"
	busy={saving}
>
	<form id="profile-form" class="stack" onsubmit={save} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<div class="grid-2">
			<Field label="Name" for="p-name" error={form.name !== '' || !error ? null : nameProblem}>
				<input id="p-name" class="input" bind:value={form.name} maxlength="80" disabled={saving || readOnly} autocomplete="off" />
			</Field>
			<Field label="Description" for="p-description" optional>
				<input id="p-description" class="input" bind:value={form.description} maxlength="500" disabled={saving || readOnly} autocomplete="off" />
			</Field>
		</div>
		<Field
			label="Presets"
			hint={formUsesPresets
				? 'The chosen presets add up: every platform in any of them is on, every other platform is off. A platform set to on or off below still wins.'
				: 'Choose presets to turn platforms on by kind; the platforms they hold are on and every other platform is off. Without any, the setting below decides.'}
		>
			<div class="chips presets" role="group" aria-label="Presets">
				{#each data.presets as preset (preset.id)}
					{@const chosen = form.platforms.presets.includes(preset.id)}
					<button
						type="button"
						class={['preset-chip', chosen && 'chosen']}
						aria-pressed={chosen}
						title={`${preset.description} ${pluralize(preset.platforms.length, 'platform')}.`}
						disabled={saving || readOnly}
						onclick={() => setPreset(preset.id, !chosen)}
					>
						{preset.label}
						<span class="count">{preset.platforms.length}</span>
					</button>
				{/each}
			</div>
		</Field>
		<Field
			label="Platforms this profile does not name"
			for="p-default"
			hint={formUsesPresets
				? 'The presets decide these: platforms in a chosen preset are on, the rest off.'
				: DEFAULT_LABELS[form.platforms.default].hint}
		>
			<select id="p-default" class="select" bind:value={form.platforms.default} disabled={saving || readOnly || formUsesPresets}>
				{#each PLATFORM_DEFAULTS as value (value)}
					<option {value}>{DEFAULT_LABELS[value as PlatformDefault].label}</option>
				{/each}
			</select>
		</Field>
		<div class="row-between">
			<input class="input search" type="search" placeholder="Find a platform by name, host or media" bind:value={search} aria-label="Find a platform" />
			{#if !readOnly}
				<span class="row">
					<span class="faint small">{pluralize(visible.length, 'platform')} shown:</span>
					<Button size="sm" variant="ghost" onclick={() => setVisible('on')} disabled={saving}>All on</Button>
					<Button size="sm" variant="ghost" onclick={() => setVisible('off')} disabled={saving}>All off</Button>
					<Button size="sm" variant="ghost" onclick={() => setVisible('inherit')} disabled={saving}>All default</Button>
				</span>
			{/if}
		</div>
		<div class="table-wrap inner">
			<table class="table platforms">
				<thead>
					<tr><th>Platform</th><th>Media</th><th>Setting</th><th>Amounts to</th></tr>
				</thead>
				<tbody>
					{#each visible as platform (platform.id)}
						{@const toggle = toggleOf(form.platforms, platform.id)}
						{@const result = formPreview[platform.id]}
						<tr>
							<td>
								<div class="stack-sm" style="gap:0">
									<span class="strong">{platform.name}</span>
									<code class="small">{platform.id}</code>
								</div>
							</td>
							<td>
								<div class="chips">
									{#each platform.media as m (m)}<span class="chip media" title={MEDIA_HINTS[m]}>{MEDIA_LABELS[m]}</span>{/each}
								</div>
							</td>
							<td>
								<div class="segmented" role="radiogroup" aria-label={`Setting for ${platform.name}`}>
									{#each TOGGLES as option (option.value)}
										<button
											type="button"
											role="radio"
											aria-checked={toggle === option.value}
											class={['seg', toggle === option.value && 'active', option.value]}
											disabled={saving || readOnly}
											onclick={() => setToggle(platform.id, option.value)}
										>{option.label}</button>
									{/each}
								</div>
							</td>
							<td>
								{#if result === true}<Badge tone="ok" size="sm" dot>On</Badge>
								{:else if result === false}<Badge tone="danger" size="sm" dot>Off</Badge>
								{:else}<Badge size="sm" title="As the wider scope has it">Wider scope</Badge>{/if}
							</td>
						</tr>
					{/each}
					{#if visible.length === 0}
						<tr><td colspan="4"><span class="faint">No platform matches.</span></td></tr>
					{/if}
				</tbody>
			</table>
		</div>
		<p class="hint">
			On its own this profile turns {formTally.on} on and {formTally.off} off, and leaves {formTally.inherit} as the wider scope has them.
		</p>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={saving}>{readOnly ? 'Close' : 'Cancel'}</Button>
		{#if !readOnly}
			<Button variant="primary" loading={saving} onclick={() => document.querySelector<HTMLFormElement>('#profile-form')?.requestSubmit()}>{editing ? 'Save profile' : 'Add profile'}</Button>
		{/if}
	{/snippet}
</Dialog>

<style>
	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.global {
		min-width: 220px;
	}

	.search {
		max-width: 320px;
	}

	.inner {
		max-height: 420px;
		overflow: auto;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
	}

	.platforms th,
	.platforms td {
		padding: 8px 12px;
	}

	.chip.media,
	.chip.preset {
		text-transform: none;
	}

	.presets {
		gap: 6px;
	}

	.preset-chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px;
		border: 1px solid var(--border);
		border-radius: 999px;
		background: var(--surface);
		color: var(--text-2);
		font: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}

	.preset-chip:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--text);
	}

	.preset-chip:disabled {
		cursor: default;
	}

	.preset-chip.chosen {
		background: var(--surface-3);
		border-color: var(--ok-text);
		color: var(--text);
		font-weight: 600;
	}

	.preset-chip .count {
		font-size: 11px;
		font-weight: 400;
		color: var(--text-3);
	}

	.segmented {
		display: inline-flex;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		overflow: hidden;
	}

	.seg {
		padding: 4px 10px;
		border: none;
		border-right: 1px solid var(--border);
		background: var(--surface);
		color: var(--text-2);
		font: inherit;
		font-size: 12.5px;
		cursor: pointer;
	}

	.seg:last-child {
		border-right: none;
	}

	.seg:disabled {
		cursor: default;
	}

	.seg.active {
		background: var(--surface-3);
		color: var(--text);
		font-weight: 600;
	}

	.seg.active.on {
		color: var(--ok-text);
	}

	.seg.active.off {
		color: var(--danger-text);
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 4px;
	}
</style>
