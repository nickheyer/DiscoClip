<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import { untrack } from 'svelte';
	import type { PageData } from './$types';
	import { MEDIA_LABELS, messageOf, profiles as api } from '$lib/api';
	import type { PlatformCoverage, PlatformTag, ProfileInput } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Field from '$lib/components/Field.svelte';
	import LimitFields from '$lib/components/LimitFields.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SectionNav from '$lib/components/SectionNav.svelte';
	import FormFeedback from '$lib/components/FormFeedback.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { MEDIA_HINTS } from '$lib/platforms';
	import {
		cleanProfile,
		describeLimits,
		describeScope,
		emptyProfile,
		inForceCount,
		preview,
		sameProfile,
		tally,
		toInput,
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

	const isNew = $derived(data.profile === null);
	const canEdit = $derived(session.can('manage_settings'));
	const readOnly = $derived(!canEdit);
	const platformIds = $derived(data.platforms.map((p) => p.id));
	const platformsById = $derived(new Map(data.platforms.map((p) => [p.id, p])));
	const sorted = $derived([...data.platforms].sort((a, b) => a.name.localeCompare(b.name)));
	const isGlobal = $derived(data.profile !== null && data.global?.profile_id === data.profile.id);
	const where = $derived(data.profile ? inForceCount(data.assignments, data.profile.id) : { total: 0, text: '' });
	const title = $derived(data.profile?.name ?? 'New profile');
	const refresh = () => invalidate(`app:profile:${data.id}`);

	// Profile editing

	let form = $state<ProfileInput>(emptyProfile());
	let formKey = $state(0);
	let limitsRef = $state<LimitFields | undefined>();
	$effect.pre(() => {
		form = data.profile ? toInput(data.profile) : emptyProfile();
		untrack(() => (formKey += 1));
	});
	const baseline = $derived(data.profile ? toInput(data.profile) : emptyProfile());
	const dirty = $derived(!sameProfile(form, baseline));
	const nameProblem = $derived(form.name.trim() === '' ? 'Enter a profile name.' : null);

	let saving = $state(false);
	let error = $state<string | null>(null);
	let deleting = $state(false);

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (readOnly) return;
		if (nameProblem || !limitsRef?.valid()) {
			error = nameProblem ?? 'Check the fields below.';
			return;
		}
		saving = true;
		error = null;
		const input = cleanProfile(form);
		try {
			if (data.profile) {
				await api.update(data.profile.id, input);
				toast.ok(`Profile ${input.name} saved.`);
				await refresh();
			} else {
				const created = await api.create(input);
				toast.ok(`Profile ${input.name} added.`);
				await goto(`/profiles/${created.id}`);
			}
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	function reset() {
		form = baseline;
		formKey += 1;
		error = null;
	}

	async function remove() {
		if (!data.profile) return;
		const ok = await confirm.ask({
			title: `Remove the profile ${data.profile.name}?`,
			message: where.total
				? `It is assigned to ${where.text}. These assignments will use their inherited profiles.`
				: 'This profile has no assignments.',
			confirmLabel: 'Remove profile',
			danger: true
		});
		if (!ok) return;
		deleting = true;
		try {
			await api.remove(data.profile.id);
			toast.ok(`Profile ${data.profile.name} removed.`);
			await goto('/profiles');
		} catch (cause) {
			toast.error(`Could not remove the profile: ${messageOf(cause)}`);
			deleting = false;
		}
	}

	// Platforms

	type Mode = 'inherit' | 'enabled' | 'disabled' | 'presets';
	const MODES: { value: Mode; label: string; hint: string }[] = [
		{
			value: 'inherit',
			label: 'Inherit settings',
			hint: 'Use the inherited settings unless overridden below.'
		},
		{
			value: 'enabled',
			label: 'Enable all platforms',
			hint: 'Individual exceptions take priority.'
		},
		{
			value: 'disabled',
			label: 'Disable all platforms',
			hint: 'Individual exceptions take priority.'
		},
		{
			value: 'presets',
			label: 'Choose categories',
			hint: 'Enable the selected categories. Individual exceptions take priority.'
		}
	];
	const mode = $derived<Mode>(usesPresets(form.platforms) ? 'presets' : form.platforms.default);
	let lastPresets = $state<string[]>([]);

	function setMode(next: Mode) {
		if (readOnly) return;
		if (next === 'presets') {
			const presets = lastPresets.length ? lastPresets : ['basic'];
			form.platforms = { ...form.platforms, presets: presets.filter((id) => data.presets.some((p) => p.id === id)) };
			if (form.platforms.presets.length === 0) form.platforms = { ...form.platforms, presets: [data.presets[0]?.id].filter(Boolean) as string[] };
		} else {
			if (form.platforms.presets.length) lastPresets = [...form.platforms.presets];
			form.platforms = { ...form.platforms, presets: [], default: next };
		}
	}

	function setPreset(preset: string, chosen: boolean) {
		if (readOnly) return;
		const next = withPreset(form.platforms, preset, chosen);
		// The last kind cannot be dropped from this mode. Switch modes instead.
		if (next.presets.length === 0) return;
		form.platforms = next;
	}

	function presetLabel(id: string): string {
		return data.presets.find((p) => p.id === id)?.label ?? id;
	}

	function setToggle(platform: string, toggle: Toggle) {
		if (readOnly) return;
		form.platforms = withToggle(form.platforms, platform, toggle);
	}

	const formPreview = $derived(preview(form.platforms, platformIds, data.presets));
	const formTally = $derived(tally(form.platforms, platformIds, data.presets));
	const named = $derived(
		Object.entries(form.platforms.overrides)
			.map(([id, on]) => ({ id, on, name: platformsById.get(id)?.name ?? id }))
			.sort((a, b) => a.name.localeCompare(b.name))
	);
	/** What the mode alone gives a platform, before exceptions: on, off, or the parent scope's. */
	function fromMode(platform: string): boolean | null {
		return preview({ ...form.platforms, overrides: {} }, [platform], data.presets)[platform] ?? null;
	}

	// Platform filters

	let search = $state('');
	let tagFilter = $state<PlatformTag | ''>('');
	let namedOnly = $state(false);
	const tags = $derived.by(() => {
		const seen = new Set<PlatformTag>();
		for (const p of data.platforms) for (const t of p.tags) seen.add(t);
		return data.presets.filter((preset) => seen.has(preset.id as PlatformTag));
	});
	const visible = $derived.by(() => {
		const needle = search.trim().toLowerCase();
		return sorted.filter((p) => {
			if (namedOnly && !(p.id in form.platforms.overrides)) return false;
			if (tagFilter && !p.tags.includes(tagFilter)) return false;
			if (!needle) return true;
			return (
				p.name.toLowerCase().includes(needle) ||
				p.id.includes(needle) ||
				p.hosts.some((h) => h.includes(needle))
			);
		});
	});

	function setVisible(toggle: Toggle) {
		if (readOnly) return;
		let next = form.platforms;
		for (const platform of visible) next = withToggle(next, platform.id, toggle);
		form.platforms = next;
	}

	const TOGGLES: { value: Toggle; label: string }[] = [
		{ value: 'inherit', label: 'Default' },
		{ value: 'on', label: 'On' },
		{ value: 'off', label: 'Off' }
	];

	function guildName(id: string): string {
		return data.guilds.find((g) => g.id === id)?.name ?? `server ${id}`;
	}

	const mine = $derived(data.profile && data.assignments ? data.assignments.filter((a) => a.profile_id === data.profile!.id) : []);
	const crumbs = $derived([{ label: 'Profiles', href: '/profiles' }, { label: title }]);
</script>

<svelte:head>
	<title>{title} · Profiles · DiscoClip</title>
</svelte:head>

<PageHeader {title} {crumbs}>
	{#snippet meta()}
		{#if data.profile}
			{#if data.profile.builtin}<Badge size="sm" tone="info">Built in</Badge>{/if}
			{#if isGlobal}<Badge size="sm" tone="ok" dot>Global default</Badge>{/if}
			{#if data.assignments}
				<span class="faint small">{where.total ? `Assigned to ${where.text}` : 'Not assigned'}</span>
			{/if}
			<span class="faint small">· updated <Time value={data.profile.updated_at} /></span>
		{:else}
			<span class="faint small">Assign this profile after saving.</span>
		{/if}
	{/snippet}
	{#snippet actions()}
		<Button href="/profiles" icon="arrow-left">All profiles</Button>
	{/snippet}
</PageHeader>

<SectionNav items={[{ id: 'profile-details', label: 'Details' }, { id: 'profile-platforms', label: 'Platforms' }, { id: 'profile-limits', label: 'Limits' }, ...(data.profile && data.assignments ? [{ id: 'profile-assignments', label: 'Assignments' }] : [])]} />

<form class="layout" onsubmit={save} novalidate>
	<div class="stack-lg main">
		{#if error}
			<FormFeedback message={error} />
		{/if}

		<section class="card" id="profile-details" tabindex="-1">
			<div class="card-header"><h2>Profile</h2></div>
			<div class="card-body form-stack">
				{#key formKey}
					<Field label="Name" for="p-name" error={error && nameProblem ? nameProblem : null}>
						<input id="p-name" class="input" bind:value={form.name} maxlength="80" disabled={saving || readOnly} autocomplete="off" />
					</Field>
					<Field label="Description" for="p-description" optional hint="Shown when choosing a profile.">
						<input id="p-description" class="input" bind:value={form.description} maxlength="500" disabled={saving || readOnly} autocomplete="off" />
					</Field>
				{/key}
			</div>
		</section>

		<section class="card" id="profile-platforms" tabindex="-1">
			<div class="card-header">
				<div>
					<h2>Platforms</h2>
					<p class="small muted">{formTally.on} enabled · {formTally.off} disabled · {formTally.inherit} inherited</p>
					<p class="hint">Choose the platforms this profile allows.</p>
				</div>
			</div>
			<div class="card-body stack">
				<fieldset class="modes"><legend class="sr-only">Default platform access</legend>
					{#each MODES as option (option.value)}
						<label class={['radio', 'mode', mode === option.value && 'chosen']}>
							<input type="radio" name="mode" value={option.value} checked={mode === option.value} onchange={() => setMode(option.value)} disabled={saving || readOnly} />
							<span>
								<span class="strong">{option.label}</span>
								<span class="hint">{option.hint}</span>
							</span>
						</label>
					{/each}
				</fieldset>
				{#if mode === 'presets'}
					<div class="chips presets" role="group" aria-label="Kinds">
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
				{/if}

				<div class="exceptions stack-sm">
					<div class="row-between">
						<div>
							<span class="strong">Exceptions</span>
							<p class="hint">Set access for individual platforms.</p>
						</div>
						{#if named.length}
							<span class="faint small">{pluralize(named.length, 'platform')} named</span>
						{/if}
					</div>
					{#if named.length}
						<div class="chips">
							{#each named as item (item.id)}
								<span class={['chip', 'named', item.on ? 'on' : 'off']}>
									{item.name}
									<span class="faint">{item.on ? 'on' : 'off'}</span>
									{#if !readOnly}
										<button type="button" class="unname" title={`Clear exception for ${item.name}`} aria-label={`Clear exception for ${item.name}`} disabled={saving} onclick={() => setToggle(item.id, 'inherit')}>×</button>
									{/if}
								</span>
							{/each}
						</div>
					{:else}
						<span class="faint small">No exceptions.</span>
					{/if}
				</div>

				<details class="browser">
					<summary>Platform exceptions</summary>
					<div class="row-between filters">
						<div class="row filter-row">
							<Field label="Find a platform" for="profile-search"><input id="profile-search" class="input search" type="search" placeholder="Name or website" bind:value={search} /></Field>
							<Field label="Category" for="profile-category"><select id="profile-category" class="select kind" bind:value={tagFilter}>
								<option value="">All categories</option>
								{#each tags as preset (preset.id)}
									<option value={preset.id}>{preset.label}</option>
								{/each}
							</select></Field>
							<label class="checkbox inline"><input type="checkbox" bind:checked={namedOnly} /><span>Exceptions only</span></label>
						</div>
						{#if !readOnly}
							<span class="row">
								<span class="faint small">{pluralize(visible.length, 'platform')} shown:</span>
								<Button size="sm" variant="ghost" onclick={() => setVisible('on')} disabled={saving || visible.length === 0}>All on</Button>
								<Button size="sm" variant="ghost" onclick={() => setVisible('off')} disabled={saving || visible.length === 0}>All off</Button>
								<Button size="sm" variant="ghost" onclick={() => setVisible('inherit')} disabled={saving || visible.length === 0}>Clear exceptions</Button>
							</span>
						{/if}
					</div>
					<div class="table-wrap inner">
						<table class="table platforms">
							<thead>
								<tr><th>Platform</th><th>Media</th><th>Setting</th><th>Effective settings</th></tr>
							</thead>
							<tbody>
								{#each visible as platform (platform.id)}
									{@const toggle = toggleOf(form.platforms, platform.id)}
									{@const result = formPreview[platform.id]}
									{@const base = fromMode(platform.id)}
									<tr class={[toggle !== 'inherit' && 'is-named']}>
										<td>
											<div class="stack-sm" style="gap:0">
												<span class="strong">{platform.name}</span>
												<span class="faint small">{platform.hosts.slice(0, 2).join(', ')}{platform.hosts.length > 2 ? ', …' : ''}</span>
											</div>
										</td>
										<td>
											<div class="chips">
												{#each platform.media as m (m)}<span class="chip media" title={MEDIA_HINTS[m]}>{MEDIA_LABELS[m]}</span>{/each}
											</div>
										</td>
										<td>
											<select class="select platform-choice" aria-label={`Access for ${platform.name}`} value={toggle} disabled={saving || readOnly} onchange={(event) => setToggle(platform.id, event.currentTarget.value as Toggle)}>
												{#each TOGGLES as option (option.value)}<option value={option.value}>{option.label}</option>{/each}
											</select>
										</td>
										<td>
											{#if result === true}<Badge tone="ok" size="sm" dot>On</Badge>
											{:else if result === false}<Badge tone="danger" size="sm" dot>Off</Badge>
											{:else}<Badge size="sm" title="Inherited">Inherited</Badge>{/if}
										</td>
									</tr>
								{/each}
								{#if visible.length === 0}
									<tr><td colspan="4"><span class="faint">No platform matches.</span></td></tr>
								{/if}
							</tbody>
						</table>
					</div>
				</details>
			</div>
		</section>

		<section class="card" id="profile-limits" tabindex="-1">
			<div class="card-header">
				<div>
					<h2>Limits</h2>
					<p class="hint">Leave blank to inherit each limit. Server limits always apply.</p>
				</div>
			</div>
			<div class="card-body">
				{#key formKey}
					<LimitFields id="p-limits" bind:value={form.limits} bind:this={limitsRef} disabled={saving || readOnly} />
				{/key}
			</div>
		</section>

		{#if data.profile && data.assignments}
			<section class="card" id="profile-assignments" tabindex="-1">
				<div class="card-header">
					<div>
						<h2>Assignments</h2>
						<p class="hint">Assign this profile from Profiles or a Discord server page.</p>
					</div>
				</div>
				<div class="card-body">
					{#if mine.length === 0}
						<span class="faint">No assignments.</span>
					{:else}
						<ul class="plain scopes">
							{#each mine as assignment (JSON.stringify(assignment.scope))}
								<li class="row-between">
									<span>
										{#if assignment.scope.kind === 'global'}
											<a href="/profiles">Global default</a>
										{:else}
											{@const scope = assignment.scope}
											<span>{describeScope(scope)} in </span><a href={`/guilds`}>{guildName(scope.guild_id)}</a>
										{/if}
									</span>
									<span class="faint small">since <Time value={assignment.updated_at} /></span>
								</li>
							{/each}
						</ul>
					{/if}
				</div>
			</section>
		{/if}

		{#if canEdit}
			<div class="form-actions">
				<p class="hint" role="status">{dirty ? 'Unsaved changes' : isNew ? 'Assign this profile after saving.' : 'All changes saved'}</p>
				<div class="row">
					<Button variant="ghost" onclick={reset} disabled={!dirty || saving}>Discard changes</Button>
					<Button type="submit" variant="primary" loading={saving} disabled={!dirty && !isNew}>{isNew ? 'Add profile' : 'Save profile'}</Button>
				</div>
			</div>
		{/if}

		{#if canEdit && data.profile && !data.profile.builtin}
			<section class="card card-danger">
				<div class="card-header"><h2>Remove profile</h2></div>
				<div class="card-body row-between">
					<p class="muted">{isGlobal ? 'Choose another default profile before deleting this one.' : 'Assignments will use their inherited profiles.'}</p>
					<Button variant="danger" icon="trash" loading={deleting} disabled={isGlobal} onclick={remove}>Remove profile</Button>
				</div>
			</section>
		{/if}
	</div>

</form>

<style>
	.platform-choice { min-width: 128px; }
	.browser summary { font-weight: 600; color: var(--accent-text); margin-bottom: 12px; }
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		max-width: 960px;
		gap: 20px;
		align-items: start;
	}


	.modes {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 10px;
	}

	.mode {
		padding: 10px 12px;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		background: var(--surface);
	}

	.mode.chosen {
		border-color: var(--accent);
		background: var(--accent-soft);
	}

	.presets {
		gap: 6px;
	}

	.preset-chip {
		min-height: 36px;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 10px;
		border: 1px solid var(--border);
		border-radius: 999px;
		background: var(--surface);
		color: var(--text-2);
		font: inherit;
		font-size: 13px;
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
		background: var(--accent-soft);
		border-color: var(--accent);
		color: var(--text);
		font-weight: 600;
	}

	.preset-chip .count {
		font-size: 13px;
		font-weight: 400;
		color: var(--text-3);
	}

	.exceptions {
		padding: 12px 14px;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		background: var(--surface-2);
	}

	.chip.named {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		text-transform: none;
	}

	.chip.named.on {
		border-color: var(--ok);
	}

	.chip.named.off {
		border-color: var(--danger);
	}

	.unname {
		min-width: 28px;
		min-height: 28px;
		padding: 0 2px;
		border: none;
		background: none;
		color: var(--text-3);
		font: inherit;
		line-height: 1;
		cursor: pointer;
	}

	.unname:hover:not(:disabled) {
		color: var(--danger-text);
	}

	.filters {
		margin-bottom: 8px;
	}

	.filter-row {
		flex-wrap: wrap;
		align-items: end;
	}

	.search {
		width: min(100%, 260px);
		flex: none;
	}

	.select.kind {
		width: 170px;
		flex: none;
	}

	.checkbox.inline {
		align-items: center;
		white-space: nowrap;
	}


	.checkbox.inline input {
		margin: 0;
	}

	.inner {
		max-height: 560px;
		overflow: auto;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
	}

	.platforms th,
	.platforms td {
		padding: 8px 12px;
	}

	.platforms tr.is-named td {
		background: color-mix(in srgb, var(--accent-soft) 45%, transparent);
	}

	.chip.media {
		text-transform: none;
	}








	.scopes li {
		padding: 6px 0;
		border-bottom: 1px solid var(--border);
	}

	.scopes li:last-child {
		border-bottom: none;
	}

	@media (max-width: 900px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}

		.modes {
			grid-template-columns: minmax(0, 1fr);
		}

		.search {
			width: 100%;
		}
	}
</style>
