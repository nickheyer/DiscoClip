<script lang="ts">
	import CheckIcon from '@lucide/svelte/icons/check';
	import { SegmentedControl, Switch } from '@skeletonlabs/skeleton-svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { platforms as platformsApi, profiles as profilesApi } from '$lib/api/endpoints';
	import type {
		PlatformCoverage,
		Preset,
		Profile,
		ProfileInput,
		ServerLimits
	} from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status, { type Tone } from '$lib/components/Status.svelte';
	import { EMPTY, bytes, clock, number, parseClock } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const id = $derived(page.params.id ?? 'new');
	const isNew = $derived(id === 'new');

	/** What the profile does with the platforms it holds no exception for. */
	type Access = 'inherit' | 'enabled' | 'disabled' | 'presets';
	const ACCESS_OPTIONS: [Access, string][] = [
		['inherit', 'Inherit'],
		['enabled', 'All platforms on'],
		['disabled', 'All platforms off'],
		['presets', 'Chosen presets']
	];
	const ACCESS_VALUES: Access[] = ACCESS_OPTIONS.map(([value]) => value);

	/** Where anything left unset here comes from instead. */
	const ONE_LEVEL_UP = 'the profile assigned one level up';
	const CHAIN =
		"a member's profile falls back to the channel's, a channel's to the Discord server's, and a Discord server's to the global default";
	const LIMITS_HELP = `A blank limit takes its value from ${ONE_LEVEL_UP}: ${CHAIN}. Each placeholder shows this DiscoClip server's own limit, which caps every profile.`;
	const ACCESS_HELP = `Inherit keeps each platform as ${ONE_LEVEL_UP} has it: ${CHAIN}.`;

	type Result = 'on' | 'off' | 'inherit';
	const RESULT: Record<Result, { label: string; tone: Tone; title: string }> = {
		on: {
			label: 'On',
			tone: 'success',
			title: 'This profile takes links from the platform.'
		},
		off: {
			label: 'Off',
			tone: 'surface',
			title: 'This profile refuses links from the platform.'
		},
		inherit: {
			label: 'Inherit',
			tone: 'secondary',
			title: `The platform stays as ${ONE_LEVEL_UP} has it.`
		}
	};

	let profile = $state<Profile | null>(null);
	let presets = $state<Preset[]>([]);
	let coverage = $state<PlatformCoverage[]>([]);
	let serverLimits = $state<ServerLimits | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let saving = $state(false);
	let errors = $state<Record<string, string>>({});

	let name = $state('');
	let description = $state('');
	let maxSourceMb = $state('');
	let maxDuration = $state('');
	let refuseLive = $state(false);
	let maxHeight = $state('');
	let access = $state<Access>('inherit');
	let chosenPresets = $state<string[]>([]);
	let overrides = $state<Record<string, boolean>>({});
	let filter = $state('');
	let tag = $state('');
	let exceptionsOnly = $state(false);
	/** The form as it was loaded, so the action bar knows whether anything changed. */
	let loadedForm = $state('');

	const canEdit = $derived(session.can('manage_settings'));

	/** Every value of the form as one comparable string. */
	function snapshot(): string {
		return JSON.stringify({
			name: name.trim(),
			description: description.trim(),
			maxSourceMb: maxSourceMb.trim(),
			maxDuration: refuseLive ? '' : maxDuration.trim(),
			refuseLive,
			maxHeight: maxHeight.trim(),
			access,
			presets: access === 'presets' ? [...chosenPresets].sort() : [],
			overrides: Object.fromEntries(
				Object.entries(overrides).sort(([a], [b]) => a.localeCompare(b))
			)
		});
	}

	function fill(source: Profile | null) {
		name = source?.name ?? '';
		description = source?.description ?? '';
		const l = source?.limits;
		maxSourceMb = l?.max_source_bytes
			? String(Math.round((l.max_source_bytes / 1024 / 1024) * 100) / 100)
			: '';
		refuseLive = l?.max_duration_secs === 0;
		maxDuration = l?.max_duration_secs ? String(l.max_duration_secs) : '';
		maxHeight = l?.max_height ? String(l.max_height) : '';
		chosenPresets = [...(source?.platforms?.presets ?? [])];
		access = chosenPresets.length > 0 ? 'presets' : (source?.platforms?.default ?? 'inherit');
		overrides = { ...(source?.platforms?.overrides ?? {}) };
		loadedForm = snapshot();
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const [presetList, platformList, loaded, all] = await Promise.all([
				profilesApi.presets(),
				platformsApi.list(),
				isNew ? Promise.resolve(null) : profilesApi.get(id),
				isNew ? profilesApi.list() : Promise.resolve(null)
			]);
			if (current !== requestId) return;
			// Every profile carries the server's own limits. A new profile reads them from
			// the profiles that already exist; the server always keeps its built-in one.
			const caps = (loaded ?? all?.[0])?.server_limits;
			if (!caps) throw new Error('The server sent no profile to read its limits from.');
			presets = presetList;
			coverage = platformList;
			profile = loaded;
			serverLimits = caps;
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

	/** The engine's caps, read once the page has loaded. */
	const caps = $derived.by(() => {
		if (!serverLimits) throw new Error('The server limits are read only once the page has loaded.');
		return serverLimits;
	});
	const sizeHint = $derived(`Server limit ${bytes(caps.max_source_bytes)}`);
	const durationHint = $derived(
		refuseLive
			? 'Zero, from the switch below'
			: caps.max_duration_secs === null
				? 'No server limit'
				: `Server limit ${clock(caps.max_duration_secs)}`
	);
	const heightHint = $derived(`Server limit ${number(caps.max_height)} px`);

	const presetPlatforms = $derived(
		new Set(presets.filter((p) => chosenPresets.includes(p.id)).flatMap((p) => p.platforms))
	);

	function resultFor(platformId: string): Result {
		const override = overrides[platformId];
		if (override !== undefined) return override ? 'on' : 'off';
		if (access === 'presets') return presetPlatforms.has(platformId) ? 'on' : 'off';
		return access === 'inherit' ? 'inherit' : access === 'enabled' ? 'on' : 'off';
	}

	function setOverride(platformId: string, value: 'inherit' | 'on' | 'off') {
		const next = { ...overrides };
		if (value === 'inherit') delete next[platformId];
		else next[platformId] = value === 'on';
		overrides = next;
	}

	function chooseAccess(next: string | null) {
		if (!next || !ACCESS_VALUES.includes(next as Access)) return;
		access = next as Access;
		if (access !== 'presets') chosenPresets = [];
	}

	function togglePreset(presetId: string) {
		chosenPresets = chosenPresets.includes(presetId)
			? chosenPresets.filter((p) => p !== presetId)
			: [...chosenPresets, presetId];
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
			if (exceptionsOnly && overrides[p.id] === undefined) return false;
			return true;
		})
	);
	const overrideCount = $derived(Object.keys(overrides).length);
	const emptyRow = $derived(
		exceptionsOnly && overrideCount === 0
			? 'No exceptions yet. Set a platform to Always on or Always off to add one.'
			: 'No platform matches.'
	);

	const presetsMissing = $derived(access === 'presets' && chosenPresets.length === 0);
	const dirty = $derived(snapshot() !== loadedForm);
	const canSave = $derived(!saving && !presetsMissing && (isNew ? name.trim().length > 0 : dirty));

	function build(): ProfileInput | null {
		const found: Record<string, string> = {};
		if (!name.trim()) found.name = 'Give the profile a name.';
		const limits: NonNullable<ProfileInput['limits']> = {};
		if (maxSourceMb.trim()) {
			const mb = Number(maxSourceMb);
			if (!Number.isFinite(mb) || mb <= 0) found.maxSourceMb = 'Enter a size above zero.';
			else limits.max_source_bytes = Math.round(mb * 1024 * 1024);
		}
		if (refuseLive) {
			limits.max_duration_secs = 0;
		} else if (maxDuration.trim()) {
			const secs = parseClock(maxDuration);
			if (secs === null || secs <= 0) found.maxDuration = 'Enter seconds or h:mm:ss above zero.';
			else limits.max_duration_secs = Math.round(secs);
		}
		if (maxHeight.trim()) {
			const px = Number(maxHeight);
			if (!Number.isInteger(px) || px <= 0) found.maxHeight = 'Enter whole pixels above zero.';
			else limits.max_height = px;
		}
		errors = found;
		if (Object.keys(found).length > 0) return null;
		return {
			name: name.trim(),
			description: description.trim(),
			limits,
			platforms: {
				default: access === 'presets' ? 'inherit' : access,
				presets: access === 'presets' ? chosenPresets : [],
				overrides
			}
		};
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (presetsMissing) return;
		const input = build();
		if (!input) return;
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
		{#if profile?.builtin}
			<div class="flex flex-wrap items-center gap-2 text-sm text-surface-600-400">
				<Status label="Built in" tone="surface" />
				<span>Ships with the server and cannot be deleted.</span>
			</div>
		{/if}
	</PageHeader>

	<form class="flex flex-col gap-6" onsubmit={save}>
		<Card title="Details">
			<fieldset disabled={!canEdit} class="grid gap-4 md:grid-cols-2">
				<Field label="Name" for="profile-name" required error={errors.name}>
					<input id="profile-name" class="input" type="text" bind:value={name} required />
				</Field>
				<Field label="Description" for="profile-description">
					<input id="profile-description" class="input" type="text" bind:value={description} />
				</Field>
			</fieldset>
		</Card>

		<Card title="Limits" description={LIMITS_HELP}>
			<fieldset disabled={!canEdit} class="space-y-4">
				<div class="grid gap-4 md:grid-cols-3">
					<Field label="Max source size" for="profile-size" error={errors.maxSourceMb}>
						<div class="field-group grid-cols-[1fr_auto]">
							<input
								id="profile-size"
								class="input"
								type="text"
								inputmode="decimal"
								placeholder={sizeHint}
								bind:value={maxSourceMb}
							/>
							<div class="label label-text preset-tonal">MB</div>
						</div>
					</Field>
					<Field label="Max duration" for="profile-duration" error={errors.maxDuration}>
						<div class="field-group grid-cols-[1fr_auto]">
							<input
								id="profile-duration"
								class="input"
								type="text"
								placeholder={durationHint}
								bind:value={maxDuration}
								disabled={refuseLive}
							/>
							<div class="label label-text preset-tonal">h:mm:ss</div>
						</div>
					</Field>
					<Field label="Max height" for="profile-height" error={errors.maxHeight}>
						<div class="field-group grid-cols-[1fr_auto]">
							<input
								id="profile-height"
								class="input"
								type="text"
								inputmode="numeric"
								placeholder={heightHint}
								bind:value={maxHeight}
							/>
							<div class="label label-text preset-tonal">px</div>
						</div>
					</Field>
				</div>
				<div class="space-y-1">
					<Switch
						checked={refuseLive}
						onCheckedChange={(details) => (refuseLive = details.checked)}
						disabled={!canEdit}
					>
						<Switch.Control><Switch.Thumb /></Switch.Control>
						<Switch.Label>Refuse live streams and anything with a running time</Switch.Label>
						<Switch.HiddenInput />
					</Switch>
					<p class="text-sm text-surface-600-400">
						This sets the maximum duration to zero, so only media with no running time, such as
						images and other files, gets through.
					</p>
				</div>
			</fieldset>
		</Card>

		<Card title="Platforms" description={ACCESS_HELP}>
			<div class="space-y-5">
				<fieldset disabled={!canEdit} class="space-y-3">
					<!-- Skeleton's SegmentedControl: one of four ways to treat every platform. -->
					<SegmentedControl
						value={access}
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

					{#if access === 'presets'}
						<div class="space-y-2">
							<p class="text-sm text-surface-600-400">
								Platforms in a chosen preset are on. Every other platform is off.
							</p>
							<!-- Skeleton filter chips: each preset toggles on and off. -->
							<div class="flex flex-wrap gap-2">
								{#each presets as preset (preset.id)}
									{@const chosen = chosenPresets.includes(preset.id)}
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
							{#if presetsMissing}
								<p class="text-sm text-error-600-400" role="alert">Choose at least one preset</p>
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
							<SearchInput
								bind:value={filter}
								placeholder="Find a platform"
								debounce={0}
								class="w-64"
							/>
						</div>
					</div>

					<!-- Skeleton's table: every platform with what this profile decides for it. -->
					<div class="max-h-[36rem] table-wrap overflow-y-auto">
						<table class="table">
							<thead class="sticky top-0 z-10 bg-surface-100-900">
								<tr>
									<th>Platform</th>
									<th
										title="What this profile decides for the platform after presets and exceptions"
									>
										Result
									</th>
									<th class="w-40">Exception</th>
								</tr>
							</thead>
							<tbody class="[&>tr]:hover:preset-tonal">
								{#each shown as platform (platform.id)}
									{@const result = resultFor(platform.id)}
									{@const override = overrides[platform.id]}
									<tr>
										<td class="align-top">
											<p class="font-medium">{platform.name}</p>
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
												{:else}
													<span>{EMPTY}</span>
												{/if}
											</p>
										</td>
										<td class="align-top">
											<span title={RESULT[result].title}>
												<Status label={RESULT[result].label} tone={RESULT[result].tone} />
											</span>
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
										<td colspan="3" class="py-6 text-center text-surface-600-400">{emptyRow}</td>
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
