<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import { SegmentedControl, Switch } from '@skeletonlabs/skeleton-svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import {
		frontends as frontendsApi,
		platforms as platformsApi,
		profiles as profilesApi
	} from '$lib/api/endpoints';
	import type {
		EffectivePolicy,
		Frontend,
		PlatformCoverage,
		Preset,
		Profile
	} from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import LanguageSelect from '$lib/components/LanguageSelect.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status, { type Tone } from '$lib/components/Status.svelte';
	import DedupeFields from '$lib/components/profile/DedupeFields.svelte';
	import DeliveryFields from '$lib/components/profile/DeliveryFields.svelte';
	import IntakeFields from '$lib/components/profile/IntakeFields.svelte';
	import LimitsFields from '$lib/components/profile/LimitsFields.svelte';
	import MessageFields from '$lib/components/profile/MessageFields.svelte';
	import OutputFields from '$lib/components/profile/OutputFields.svelte';
	import { number } from '$lib/format';
	import { languageName } from '$lib/languages';
	import { inherit } from '$lib/policy';
	import { draftOf, inputOf, type Draft, type PlatformAccess, type Problems } from '$lib/profile';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? 'new');
	const isNew = $derived(id === 'new');

	const ACCESS_OPTIONS: [PlatformAccess, string][] = [
		['inherit', 'Inherit'],
		['enabled', 'All platforms on'],
		['disabled', 'All platforms off'],
		['presets', 'Chosen presets']
	];
	const ACCESS_VALUES: PlatformAccess[] = ACCESS_OPTIONS.map(([value]) => value);

	type Result = 'on' | 'off' | 'inherit';
	const RESULT: Record<Result, { label: string; tone: Tone }> = {
		on: { label: 'On', tone: 'success' },
		off: { label: 'Off', tone: 'surface' },
		inherit: { label: 'Inherit', tone: 'secondary' }
	};

	let profile = $state<Profile | null>(null);
	let presets = $state<Preset[]>([]);
	let coverage = $state<PlatformCoverage[]>([]);
	let views = $state<Frontend[]>([]);
	let effective = $state<EffectivePolicy | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let saving = $state(false);
	let problems = $state<Problems>({});

	let draft = $state<Draft>(draftOf(null));
	let filter = $state('');
	let tag = $state('');
	let exceptionsOnly = $state(false);
	/** The form as it was loaded, so the action bar knows whether anything changed. */
	let loadedForm = $state('');

	const canEdit = $derived(session.can('manage_settings'));
	const builtin = $derived(profile?.builtin ?? false);

	/** Every value of the form as one comparable string. */
	const snapshot = () => JSON.stringify($state.snapshot(draft));

	function fill(source: Profile | null) {
		draft = draftOf(source);
		problems = {};
		loadedForm = snapshot();
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [presetList, platformList, loaded, settled, viewList] = await Promise.all([
				profilesApi.presets(),
				platformsApi.list(),
				isNew ? Promise.resolve(null) : profilesApi.get(id),
				profilesApi.effective(),
				canEdit ? frontendsApi.list() : Promise.resolve([])
			]);
			if (current !== requestId) return;
			presets = presetList;
			coverage = platformList;
			profile = loaded;
			effective = settled;
			views = viewList;
			fill(loaded);
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void id;
		void load();
	});

	/** What the whole server runs under, read once the page has loaded */
	const settled = $derived.by(() => {
		if (!effective) throw new Error('The effective policy is read only once the page has loaded.');
		return effective;
	});

	const presetPlatforms = $derived(
		new Set(presets.filter((p) => draft.presets.includes(p.id)).flatMap((p) => p.platforms))
	);

	/** The platforms the engine leaves off until a profile names them */
	const offUntilNamed = $derived(
		new Set(coverage.filter((p) => !p.on_by_default).map((p) => p.id))
	);

	function resultFor(platformId: string): Result {
		const override = draft.overrides[platformId];
		if (override !== undefined) return override ? 'on' : 'off';
		if (draft.access === 'presets') return presetPlatforms.has(platformId) ? 'on' : 'off';
		if (draft.access === 'disabled') return 'off';
		if (draft.access === 'enabled') return offUntilNamed.has(platformId) ? 'inherit' : 'on';
		return 'inherit';
	}

	function setOverride(platformId: string, value: 'inherit' | 'on' | 'off') {
		const next = { ...draft.overrides };
		if (value === 'inherit') delete next[platformId];
		else next[platformId] = value === 'on';
		draft.overrides = next;
	}

	function chooseAccess(next: string | null) {
		if (!next || !ACCESS_VALUES.includes(next as PlatformAccess)) return;
		draft.access = next as PlatformAccess;
		if (draft.access !== 'presets') draft.presets = [];
	}

	function togglePreset(presetId: string) {
		draft.presets = draft.presets.includes(presetId)
			? draft.presets.filter((p) => p !== presetId)
			: [...draft.presets, presetId];
	}

	/** `1 platform`, `112 platforms`. */
	function count(n: number, one: string, many: string): string {
		return `${number(n)} ${n === 1 ? one : many}`;
	}

	const tags = $derived([...new Set(coverage.flatMap((p) => p.tags))].sort());
	const narrowed = $derived(filter.trim() !== '' || tag !== '' || exceptionsOnly);
	const shown = $derived(
		coverage.filter((p) => {
			const needle = filter.trim().toLowerCase();
			if (
				needle &&
				!p.name.toLowerCase().includes(needle) &&
				!p.id.includes(needle) &&
				!p.hosts.some((h) => h.includes(needle))
			)
				return false;
			if (tag && !p.tags.includes(tag)) return false;
			if (exceptionsOnly && draft.overrides[p.id] === undefined) return false;
			return true;
		})
	);
	const overrideCount = $derived(Object.keys(draft.overrides).length);

	const dirty = $derived(snapshot() !== loadedForm);
	const canSave = $derived(!saving && (isNew ? draft.name.trim().length > 0 : dirty));
	const audioBlank = $derived(
		builtin ? 'Any' : inherit(settled.audio_language ? languageName(settled.audio_language) : 'Any')
	);

	async function save(event: SubmitEvent) {
		event.preventDefault();
		const { input, problems: found } = inputOf(draft, builtin);
		problems = found;
		if (Object.keys(found).length > 0) return;
		saving = true;
		try {
			const saved = isNew ? await profilesApi.create(input) : await profilesApi.update(id, input);
			notify.success(isNew ? 'Profile created' : 'Profile saved', saved.name);
			await goto(resolve('/profiles'));
		} catch (err) {
			reportError(err, 'Could not save the profile');
		} finally {
			saving = false;
		}
	}

	const back = { href: resolve('/profiles'), label: 'Profiles' };
</script>

{#if error && !loading}
	<PageHeader title="Profile" {back} />
	<ErrorState {error} title="This profile could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Profile" {back} />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader title={isNew ? 'New profile' : (profile?.name ?? 'Profile')} {back}>
		{#if builtin}
			<div class="flex flex-wrap items-center gap-2 text-sm text-surface-600-400">
				<Status label="Built in" tone="surface" />
			</div>
		{/if}
	</PageHeader>

	<form class="flex flex-col gap-6" onsubmit={save}>
		<Card title="Details">
			<fieldset disabled={!canEdit} class="grid gap-4 md:grid-cols-3">
				<Field label="Name" for="profile-name" required error={problems.name}>
					<input id="profile-name" class="input" type="text" bind:value={draft.name} required />
				</Field>
				<Field label="Description" for="profile-description">
					<input
						id="profile-description"
						class="input"
						type="text"
						bind:value={draft.description}
					/>
				</Field>
				<Field label="Audio language" for="profile-audio-language">
					<LanguageSelect
						id="profile-audio-language"
						bind:value={draft.audioLanguage}
						blank={audioBlank}
						disabled={!canEdit}
					/>
				</Field>
			</fieldset>
		</Card>

		<Card title="Limits">
			<fieldset disabled={!canEdit}>
				<LimitsFields bind:value={draft.limits} effective={settled} {builtin} {problems} />
			</fieldset>
		</Card>

		<Card title="Intake">
			<fieldset disabled={!canEdit}>
				<IntakeFields bind:value={draft.intake} effective={settled} {builtin} {problems} />
			</fieldset>
		</Card>

		<Card title="Output">
			<fieldset disabled={!canEdit}>
				<OutputFields bind:value={draft.output} effective={settled} {builtin} {problems} />
			</fieldset>
		</Card>

		<Card title="Delivery">
			<fieldset disabled={!canEdit}>
				<DeliveryFields
					bind:upload={draft.upload}
					bind:value={draft.delivery}
					{views}
					effective={settled}
					{builtin}
					{problems}
				/>
			</fieldset>
		</Card>

		<Card title="Message">
			<fieldset disabled={!canEdit}>
				<MessageFields
					bind:value={draft.message}
					bind:errors={draft.errors}
					effective={settled}
					{builtin}
					{problems}
				/>
			</fieldset>
		</Card>

		<Card title="Dedupe">
			<fieldset disabled={!canEdit}>
				<DedupeFields bind:value={draft.dedupe} effective={settled} {builtin} {problems} />
			</fieldset>
		</Card>

		<Card title="Platforms">
			<div class="space-y-5">
				<fieldset disabled={!canEdit} class="space-y-3">
					<!-- Skeleton's SegmentedControl: one of four ways to treat every platform. -->
					<SegmentedControl
						value={draft.access}
						onValueChange={(details) => chooseAccess(details.value)}
						disabled={!canEdit}
						class="w-full"
					>
						<SegmentedControl.Label>Access</SegmentedControl.Label>
						<SegmentedControl.Control class="flex-wrap">
							<SegmentedControl.Indicator />
							{#each ACCESS_OPTIONS as [value, label] (value)}
								<SegmentedControl.Item {value}>
									<SegmentedControl.ItemText>{label}</SegmentedControl.ItemText>
									<SegmentedControl.ItemHiddenInput />
								</SegmentedControl.Item>
							{/each}
						</SegmentedControl.Control>
					</SegmentedControl>

					{#if draft.access === 'presets'}
						<div class="space-y-2">
							<!-- Skeleton filter chips: each preset toggles on and off. -->
							<div class="flex flex-wrap gap-2">
								{#each presets as preset (preset.id)}
									{@const chosen = draft.presets.includes(preset.id)}
									<button
										type="button"
										class="chip {chosen ? 'preset-filled' : 'preset-outlined-surface-400-600'}"
										aria-pressed={chosen}
										title={preset.description}
										onclick={() => togglePreset(preset.id)}
									>
										{#if chosen}<CheckIcon />{/if}
										<span>{preset.label}</span>
										<span class="opacity-60">{number(preset.platforms.length)}</span>
									</button>
								{/each}
							</div>
							{#if problems['platforms.presets']}
								<p class="text-xs text-error-600-400" role="alert">
									{problems['platforms.presets']}
								</p>
							{/if}
						</div>
					{/if}
				</fieldset>

				<hr class="hr" />

				<div class="space-y-3">
					<div class="flex flex-wrap items-center justify-between gap-3">
						<p class="text-sm text-surface-600-400">
							{count(coverage.length, 'platform', 'platforms')} · {count(
								overrideCount,
								'exception',
								'exceptions'
							)}{#if narrowed}
								· {number(shown.length)} shown{/if}
						</p>
						<div class="flex flex-wrap items-center gap-3">
							<Switch
								checked={exceptionsOnly}
								onCheckedChange={(details) => (exceptionsOnly = details.checked)}
							>
								<Switch.Control><Switch.Thumb /></Switch.Control>
								<Switch.Label>Exceptions only</Switch.Label>
								<Switch.HiddenInput />
							</Switch>
							<select class="select w-40" bind:value={tag} aria-label="Filter by tag">
								<option value="">Every tag</option>
								{#each tags as t (t)}
									<option value={t}>{t}</option>
								{/each}
							</select>
							<SearchInput bind:value={filter} debounce={0} class="w-64" />
						</div>
					</div>

					<!-- Skeleton's table: every platform with what this profile decides for it. -->
					<div class="max-h-[36rem] table-wrap overflow-y-auto">
						<table class="table">
							<thead class="sticky top-0 z-10 bg-surface-100-900">
								<tr>
									<th>Platform</th>
									<th>Result</th>
									<th class="w-40">Exception</th>
								</tr>
							</thead>
							<tbody class="[&>tr]:hover:preset-tonal">
								{#each shown as platform (platform.id)}
									{@const result = resultFor(platform.id)}
									{@const override = draft.overrides[platform.id]}
									<tr>
										<td class="align-top">
											<p class="flex flex-wrap items-center gap-2 font-medium">
												{platform.name}
												{#if !platform.on_by_default}
													<Status label="Off until turned on" tone="warning" />
												{/if}
											</p>
											<p class="flex flex-wrap items-baseline gap-x-2 text-sm text-surface-600-400">
												{#if platform.hosts.length > 0}
													<span class="font-mono text-xs break-words"
														>{platform.hosts.slice(0, 3).join(', ')}</span
													>
													{#if platform.hosts.length > 3}
														<span title={platform.hosts.slice(3).join(', ')}
															>+{number(platform.hosts.length - 3)} more</span
														>
													{/if}
												{/if}
											</p>
										</td>
										<td class="align-top">
											<Status label={RESULT[result].label} tone={RESULT[result].tone} />
										</td>
										<td class="align-top">
											<select
												class="select"
												value={override === undefined ? 'inherit' : override ? 'on' : 'off'}
												onchange={(event) =>
													setOverride(
														platform.id,
														event.currentTarget.value as 'inherit' | 'on' | 'off'
													)}
												aria-label="Exception for {platform.name}"
												disabled={!canEdit}
											>
												<option value="inherit">Inherit</option>
												<option value="on">Always on</option>
												<option value="off">Always off</option>
											</select>
										</td>
									</tr>
								{:else}
									<tr>
										<td colspan="3" class="py-6 text-center text-surface-600-400">
											{exceptionsOnly && overrideCount === 0
												? 'No exceptions'
												: 'No platform matches'}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				</div>
			</div>
		</Card>

		{#if canEdit}
			<div
				class="sticky bottom-[calc(5rem+env(safe-area-inset-bottom))] z-20 flex flex-wrap items-center justify-end gap-3 card preset-filled-surface-100-900 p-3 shadow-lg lg:bottom-4"
			>
				{#if dirty}
					<span class="mr-auto badge preset-tonal-warning" style="--badge-size: var(--text-xs)">
						Unsaved changes
					</span>
				{/if}
				<a href={resolve('/profiles')} class="btn preset-tonal">Cancel</a>
				<button type="submit" class="btn preset-filled-primary-500" disabled={!canSave}>
					{#if saving}<Spinner />{/if}
					{isNew ? 'Create profile' : 'Save profile'}
				</button>
			</div>
		{/if}
	</form>
{/if}
