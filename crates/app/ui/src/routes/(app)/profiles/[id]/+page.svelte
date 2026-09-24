<script lang="ts">
	import ArrowLeftIcon from '@lucide/svelte/icons/arrow-left';
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
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SearchInput from '$lib/components/SearchInput.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
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

	/** Where anything left unset here comes from instead. */
	const ONE_LEVEL_UP = 'the profile assigned one level up';
	const CHAIN =
		"a member's profile falls back to the channel's, a channel's to the Discord server's, and a Discord server's to the global default";
	const LIMITS_HELP = `A blank limit takes its value from ${ONE_LEVEL_UP}: ${CHAIN}. Each placeholder shows this DiscoClip server's own limit, which caps every profile.`;
	const ACCESS_HELP = `Inherit keeps each platform as ${ONE_LEVEL_UP} has it: ${CHAIN}.`;

	type Result = 'on' | 'off' | 'inherit';
	const RESULT: Record<Result, { label: string; class: string; title: string }> = {
		on: {
			label: 'On',
			class: 'text-success-700-300',
			title: 'This profile takes links from the platform.'
		},
		off: {
			label: 'Off',
			class: 'text-surface-600-400',
			title: 'This profile refuses links from the platform.'
		},
		inherit: {
			label: 'Inherit',
			class: 'text-secondary-700-300',
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
			? 'Zero, from the checkbox below'
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

	function chooseAccess(next: Access) {
		access = next;
		if (next !== 'presets') chosenPresets = [];
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
</script>

{#if error && !loading}
	<PageHeader title="Profile" />
	<ErrorState {error} title="This profile could not be loaded" onretry={load} />
{:else if loading}
	<PageHeader title="Profile" />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-64 placeholder animate-pulse"></div>
	</div>
{:else}
	<a
		href={resolve('/profiles')}
		class="link-body inline-flex items-center gap-1 text-sm text-surface-700-300 hover:text-surface-950-50"
	>
		<ArrowLeftIcon class="size-4" />
		All profiles
	</a>

	<PageHeader title={isNew ? 'New profile' : (profile?.name ?? 'Profile')}>
		{#if profile?.builtin}
			<p class="text-sm text-surface-600-400">Ships with the server and cannot be deleted.</p>
		{/if}
	</PageHeader>

	<form class="flex flex-col gap-6" onsubmit={save}>
		<fieldset
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-labelledby="details-heading"
			disabled={!canEdit}
		>
			<h2 id="details-heading" class="h6">Details</h2>
			<div class="grid gap-4 md:grid-cols-2">
				<Field label="Name" for="profile-name" required error={errors.name}>
					<input id="profile-name" class="input" type="text" bind:value={name} required />
				</Field>
				<Field label="Description" for="profile-description">
					<input id="profile-description" class="input" type="text" bind:value={description} />
				</Field>
			</div>
		</fieldset>

		<fieldset
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-labelledby="limits-heading"
			disabled={!canEdit}
		>
			<h2 id="limits-heading" class="h6">Limits</h2>
			<p class="text-sm text-surface-600-400">{LIMITS_HELP}</p>
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
						<div class="label preset-tonal text-sm">MB</div>
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
						<div class="label preset-tonal text-sm">h:mm:ss</div>
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
						<div class="label preset-tonal text-sm">px</div>
					</div>
				</Field>
			</div>
			<div class="space-y-1">
				<label class="flex items-start gap-2 text-sm">
					<input class="mt-0.5 checkbox" type="checkbox" bind:checked={refuseLive} />
					<span>Refuse live streams and anything with a running time</span>
				</label>
				<p class="pl-6 text-sm text-surface-600-400">
					This sets the maximum duration to zero, so only media with no running time, such as images
					and other files, gets through.
				</p>
			</div>
		</fieldset>

		<section
			class="space-y-4 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6"
			aria-labelledby="platforms-heading"
		>
			<h2 id="platforms-heading" class="h6">Platforms</h2>

			<fieldset class="space-y-2" disabled={!canEdit}>
				<legend class="pb-1 text-sm font-medium">Access</legend>
				{#each ACCESS_OPTIONS as [value, label] (value)}
					<label class="flex items-center gap-2 text-sm">
						<input
							class="radio"
							type="radio"
							name="platform-access"
							{value}
							checked={access === value}
							onchange={() => chooseAccess(value)}
						/>
						{label}
					</label>
				{/each}
				<p class="text-sm text-surface-600-400">{ACCESS_HELP}</p>

				{#if access === 'presets'}
					<div class="space-y-2 pt-2">
						<p class="text-sm text-surface-600-400">
							Platforms in a chosen preset are on. Every other platform is off.
						</p>
						<div class="flex flex-wrap gap-2">
							{#each presets as preset (preset.id)}
								<button
									type="button"
									class="chip {chosenPresets.includes(preset.id)
										? 'preset-filled'
										: 'preset-tonal'}"
									aria-pressed={chosenPresets.includes(preset.id)}
									title={preset.description}
									onclick={() => togglePreset(preset.id)}
								>
									{preset.label}
									<span class="opacity-70">{number(preset.platforms.length)}</span>
								</button>
							{/each}
						</div>
						{#if presetsMissing}
							<p class="text-sm text-error-700-300" role="alert">Choose at least one preset</p>
						{/if}
					</div>
				{/if}
			</fieldset>

			<div class="space-y-2">
				<div class="flex flex-wrap items-center justify-between gap-3">
					<p class="text-sm text-surface-600-400">
						{count(coverage.length, 'platform', 'platforms')} · {count(
							overrideCount,
							'exception',
							'exceptions'
						)}{#if narrowed}
							· {number(shown.length)} shown{/if}
					</p>
					<div class="flex flex-wrap items-center gap-2">
						<label class="flex items-center gap-2 text-sm">
							<input class="checkbox" type="checkbox" bind:checked={exceptionsOnly} />
							Exceptions only
						</label>
						<select class="select-sm select w-40" bind:value={tag} aria-label="Filter by tag">
							<option value="">Every tag</option>
							{#each tags as t (t)}
								<option value={t}>{t}</option>
							{/each}
						</select>
						<SearchInput
							bind:value={filter}
							placeholder="Find a platform"
							debounce={0}
							class="w-56"
						/>
					</div>
				</div>

				<table class="table w-full">
					<thead class="sticky top-16 z-10 bg-surface-100-900 lg:top-20">
						<tr>
							<th class="px-3 py-2 text-left">Platform</th>
							<th
								class="px-3 py-2 text-left"
								title="What this profile decides for the platform after presets and exceptions"
								>Result</th
							>
							<th class="w-36 px-3 py-2 text-left">Exception</th>
						</tr>
					</thead>
					<tbody>
						{#each shown as platform (platform.id)}
							{@const result = resultFor(platform.id)}
							{@const override = overrides[platform.id]}
							<tr>
								<td class="px-3 py-2 align-top">
									<p class="font-medium">{platform.name}</p>
									<p class="flex flex-wrap items-baseline gap-x-2 text-sm text-surface-600-400">
										{#if platform.hosts.length > 0}
											<span class="font-mono text-xs break-all"
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
								<td class="px-3 py-2 align-top">
									<span class={RESULT[result].class} title={RESULT[result].title}
										>{RESULT[result].label}</span
									>
								</td>
								<td class="px-3 py-2 align-top">
									<select
										class="select-sm select w-full"
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
							<tr
								><td colspan="3" class="px-3 py-6 text-center text-surface-600-400">{emptyRow}</td
								></tr
							>
						{/each}
					</tbody>
				</table>
			</div>
		</section>

		{#if canEdit}
			<div
				class="sticky bottom-[calc(5rem+env(safe-area-inset-bottom))] z-20 -mx-4 flex flex-wrap items-center justify-end gap-3 border-t border-surface-200-800 bg-surface-50-950/95 px-4 py-3 backdrop-blur sm:-mx-6 sm:px-6 lg:bottom-0 lg:-mx-8 lg:px-8 lg:pb-[max(0.75rem,env(safe-area-inset-bottom))]"
			>
				{#if dirty}
					<p class="mr-auto text-sm text-surface-600-400">Unsaved changes</p>
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
