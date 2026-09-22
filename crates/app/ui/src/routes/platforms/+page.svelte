<script lang="ts">
	import FormFeedback from '$lib/components/FormFeedback.svelte';
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { MEDIA_KINDS, MEDIA_LABELS, PLATFORM_TAGS, messageOf, platforms } from '$lib/api';
	import type { MediaKind, PlatformCoverage, PlatformTag } from '$lib/api';
	import type { CookieFormat } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { formatDuration, pluralize } from '$lib/format';
	import {
		COVERAGE_LABELS,
		FIXTURE_STATUS_LABELS,
		MEDIA_HINTS,
		SESSION_LABELS,
		TAG_LABELS,
		coverageState,
		coverageTone,
		describeFound,
		fixtureTone,
		sessionSummary
	} from '$lib/platforms';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const canRun = $derived(session.can('manage_jobs'));
	const canManageSessions = $derived(session.can('manage_settings'));

	// Sessions: cookies imported, checked and cleared per platform.

	let importFor = $state<PlatformCoverage | null>(null);
	let importFormat = $state<CookieFormat>('netscape');
	let importText = $state('');
	let importDomain = $state('');
	let importing = $state(false);
	let importError = $state<string | null>(null);
	let checking = $state<string | null>(null);
	let clearing = $state<string | null>(null);

	function openImport(platform: PlatformCoverage) {
		importFor = platform;
		importFormat = 'netscape';
		importText = '';
		importDomain = platform.hosts.find((h) => h !== '*') ?? '';
		importError = null;
	}

	async function runImport(event: SubmitEvent) {
		event.preventDefault();
		if (!importFor || !importText.trim()) return;
		importing = true;
		importError = null;
		try {
			const outcome = await platforms.importCookies(importFor.id, {
				format: importFormat,
				text: importText,
				domain: importFormat === 'header' ? importDomain.trim() || undefined : undefined
			});
			const state = sessionSummary(outcome);
			if (outcome.check_error) {
				toast.info(`Stored ${pluralize(outcome.cookies, 'cookie')} for ${outcome.name}. Session check failed: ${outcome.check_error}`);
			} else {
				toast.ok(`Stored ${pluralize(outcome.cookies, 'cookie')} for ${outcome.name}. ${state.label}.`);
			}
			importFor = null;
			await invalidate('app:platforms');
		} catch (cause) {
			importError = messageOf(cause);
		} finally {
			importing = false;
		}
	}

	async function checkSession(platform: PlatformCoverage) {
		checking = platform.id;
		try {
			const view = await platforms.checkSession(platform.id);
			toast.ok(`${view.name}: ${sessionSummary(view).label}.`);
			await invalidate('app:platforms');
		} catch (cause) {
			toast.error(`Could not check the session: ${messageOf(cause)}`);
		} finally {
			checking = null;
		}
	}

	async function clearCookies(platform: PlatformCoverage) {
		const ok = await confirm.ask({
			title: `Clear the cookies of ${platform.name}?`,
			message: 'The platform is read without an account from then on.',
			confirmLabel: 'Clear cookies',
			danger: true
		});
		if (!ok) return;
		clearing = platform.id;
		try {
			await platforms.clearCookies(platform.id);
			toast.ok(`Cleared the cookies of ${platform.name}.`);
			await invalidate('app:platforms');
		} catch (cause) {
			toast.error(`Could not clear the cookies: ${messageOf(cause)}`);
		} finally {
			clearing = null;
		}
	}
	let query = $state('');
	let kind = $state<MediaKind | ''>('');
	let tag = $state<PlatformTag | ''>('');
	let open = $state<Record<string, boolean>>({});
	let starting = $state<string | null>(null);
	let startingAll = $state(false);

	const list = $derived.by(() => {
		const needle = query.trim().toLowerCase();
		const all = [...data.platforms]
			.sort((a, b) => a.name.localeCompare(b.name))
			.filter((p) => !kind || p.media.includes(kind))
			.filter((p) => !tag || p.tags.includes(tag));
		if (!needle) return all;
		return all.filter(
			(p) =>
				p.name.toLowerCase().includes(needle) ||
				p.id.includes(needle) ||
				p.hosts.some((h) => h.includes(needle)) ||
				p.features.some((f) => f.includes(needle)) ||
				p.formats.some((f) => f.includes(needle)) ||
				p.media.some((m) => m.includes(needle)) ||
				p.tags.some((t) => t.includes(needle) || TAG_LABELS[t].label.toLowerCase().includes(needle))
		);
	});
	const byKind = $derived(
		Object.fromEntries(
			MEDIA_KINDS.map((k) => [k, data.platforms.filter((p) => p.media.includes(k)).length])
		) as Record<MediaKind, number>
	);
	const withFixtures = $derived(data.platforms.filter((p) => p.fixtures.length > 0));
	const passing = $derived(withFixtures.filter((p) => coverageState(p) === 'passing').length);
	const failing = $derived(withFixtures.filter((p) => coverageState(p) === 'failing').length);
	const needingLogin = $derived(withFixtures.filter((p) => coverageState(p) === 'login').length);
	const running = $derived(data.platforms.some((p) => p.running));
	const lastRun = $derived(
		data.platforms.reduce<string | null>(
			(latest, p) => (p.last_run_at && (!latest || p.last_run_at > latest) ? p.last_run_at : latest),
			null
		)
	);
	const formats = $derived([...new Set(data.platforms.flatMap((p) => p.formats))].sort());

	// While fixtures run, the page asks again until they are done.
	$effect(() => {
		if (!running) return;
		const timer = setInterval(() => void invalidate('app:platforms'), 2000);
		return () => clearInterval(timer);
	});

	function toggle(id: string) {
		open[id] = !open[id];
	}

	async function runOne(platform: PlatformCoverage) {
		starting = platform.id;
		try {
			await platforms.check(platform.id);
			toast.ok(`Running the fixtures of ${platform.name}.`);
			open[platform.id] = true;
			await invalidate('app:platforms');
		} catch (cause) {
			toast.error(`Could not run the fixtures: ${messageOf(cause)}`);
		} finally {
			starting = null;
		}
	}

	async function runAll() {
		startingAll = true;
		try {
			const started = await platforms.checkAll();
			toast.ok(`Running the fixtures of ${pluralize(started.platforms.length, 'platform')}.`);
			await invalidate('app:platforms');
		} catch (cause) {
			toast.error(`Could not run the fixtures: ${messageOf(cause)}`);
		} finally {
			startingAll = false;
		}
	}

	function duration(ms: number | null): string {
		if (ms === null) return '';
		return ms < 1000 ? `${ms} ms` : formatDuration(ms / 1000);
	}
</script>

<svelte:head>
	<title>Platforms · DiscoClip</title>
</svelte:head>

{#snippet sessionBadge(platform: PlatformCoverage)}
	{@const summary = sessionSummary(platform)}
	<Badge tone={summary.tone} size="sm" dot={summary.tone !== 'neutral'}>{summary.label}</Badge>
{/snippet}

<PageHeader
	title="Platforms"
	description="Supported sites, media types and connection status."
>
	{#snippet actions()}
		{#if canRun}
			<Button variant="primary" icon="play" loading={startingAll} disabled={running || withFixtures.length === 0} onclick={runAll}>
				Run all fixtures
			</Button>
		{/if}
	{/snippet}
</PageHeader>

{#if data.platforms.length === 0}
	<Empty icon="globe" title="No platforms" description="The engine has no resolvers registered." />
{:else}
	<div class="stack">
		<div class="summary">
			<div class="stat">
				<span class="stat-value">{data.platforms.length}</span>
				<span class="stat-label">{data.platforms.length === 1 ? 'platform' : 'platforms'}</span>
			</div>
			<div class="stat">
				<span class="stat-value ok">{passing}</span>
				<span class="stat-label">passing in full</span>
			</div>
			<div class="stat">
				<span class={['stat-value', failing > 0 && 'danger']}>{failing}</span>
				<span class="stat-label">with a failing fixture</span>
			</div>
			<div class="stat">
				<span class={['stat-value', needingLogin > 0 && 'warn']}>{needingLogin}</span>
				<span class="stat-label">needing a login</span>
			</div>
			<div class="stat">
				<span class="stat-value">{formats.length}</span>
				<span class="stat-label">{formats.length === 1 ? 'format' : 'formats'}</span>
			</div>
			<div class="stat wide">
				<span class="stat-value small-value kinds">
					{#each MEDIA_KINDS as k (k)}<span title={MEDIA_HINTS[k]}>{byKind[k]} {MEDIA_LABELS[k].toLowerCase()}</span>{/each}
				</span>
				<span class="stat-label">platforms resolving each kind of media</span>
			</div>
			<div class="stat wide">
				<span class="stat-value small-value">{#if lastRun}<Time value={lastRun} />{:else}never{/if}</span>
				<span class="stat-label">{running ? 'fixtures running now' : 'last fixture run'}</span>
			</div>
		</div>

		<div class="row-between">
			<div class="row">
				<Field label="Filter platforms" for="control-9098"><input id="control-9098" class="input search" type="search" placeholder="Filter by name, host, feature, format, media or tag" bind:value={query} aria-label="Filter platforms" /></Field>
				<Field label="Filter by media kind" for="control-9262"><select id="control-9262" class="select" bind:value={kind} aria-label="Filter by media kind">
					<option value="">Any media</option>
					{#each MEDIA_KINDS as k (k)}<option value={k}>{MEDIA_LABELS[k]}</option>{/each}
				</select></Field>
				<Field label="Filter by tag" for="control-9482"><select id="control-9482" class="select" bind:value={tag} aria-label="Filter by tag">
					<option value="">Any tag</option>
					{#each PLATFORM_TAGS as t (t)}<option value={t}>{TAG_LABELS[t].label}</option>{/each}
				</select></Field>
			</div>
			<span class="faint small">{pluralize(list.length, 'platform')} shown</span>
		</div>

		{#if list.length === 0}
			<Empty compact icon="search" title="Nothing matches" description="No platform matches that filter." />
		{:else}
			<div class="table-wrap">
				<table class="table">
					<thead>
						<tr>
							<th></th>
							<th>Platform</th>
							<th>Hosts</th>
							<th>Media</th>
							<th>Tags</th>
							<th>Formats</th>
							<th>Session</th>
							<th>Fixtures</th>
							<th>Last pass</th>
							<th>Last run</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each list as platform (platform.id)}
							{@const state = coverageState(platform)}
							{@const expanded = open[platform.id] ?? false}
							<tr class={[expanded && 'expanded']}>
								<td class="toggle">
									<button
										type="button"
										class="expander"
										onclick={() => toggle(platform.id)}
										aria-expanded={expanded}
										aria-controls={`platform-${platform.id}`}
										aria-label={expanded ? `Hide ${platform.name}` : `Show ${platform.name}`}
									>
										<Icon name={expanded ? 'chevron-down' : 'chevron-right'} size={14} />
									</button>
								</td>
								<td>
									<div class="stack-sm" style="gap:0">
										<span class="strong">{platform.name}</span>
										<code class="small">{platform.id}</code>
									</div>
								</td>
								<td>
									<div class="chips">
										{#each platform.hosts.slice(0, 3) as host (host)}<span class="chip">{host}</span>{/each}
										{#if platform.hosts.length > 3}<span class="chip">+{platform.hosts.length - 3}</span>{/if}
									</div>
								</td>
								<td>
									<div class="chips">
										{#each platform.media as m (m)}<span class="chip media" title={MEDIA_HINTS[m]}>{MEDIA_LABELS[m]}</span>{/each}
										{#if platform.media.length === 0}<span class="faint" title="Its links only lead on to other platforms' resolvers.">Not available</span>{/if}
									</div>
								</td>
								<td>
									<div class="chips">
										{#each platform.tags as t (t)}<span class="chip media" title={TAG_LABELS[t].hint}>{TAG_LABELS[t].label}</span>{/each}
										{#if platform.tags.length === 0}<span class="faint">Not available</span>{/if}
									</div>
								</td>
								<td>
									<div class="chips">
										{#each platform.formats as format (format)}<span class="chip">{format}</span>{/each}
										{#if platform.formats.length === 0}<span class="faint">Not available</span>{/if}
									</div>
								</td>
								<td>
									<div class="stack-sm" style="gap:2px">
										<span title={SESSION_LABELS[platform.session].hint}>{SESSION_LABELS[platform.session].label}</span>
										{#if platform.session !== 'none'}
											{@render sessionBadge(platform)}
										{/if}
									</div>
								</td>
								<td>
									<div class="row">
										<Badge tone={coverageTone(state)} size="sm" dot={state !== 'none'} pulse={state === 'running'}>{COVERAGE_LABELS[state]}</Badge>
										{#if platform.fixtures.length > 0 && platform.last_run_at}
											<span class="faint small">{platform.passed} of {platform.fixtures.length}</span>
										{:else if platform.fixtures.length > 0}
											<span class="faint small">{pluralize(platform.fixtures.length, 'link')}</span>
										{/if}
									</div>
								</td>
								<td><Time value={platform.last_pass_at} empty={platform.fixtures.length ? 'never' : 'Not available'} /></td>
								<td><Time value={platform.last_run_at} empty={platform.fixtures.length ? 'never' : 'Not available'} /></td>
								<td class="actions">
									{#if canRun && platform.fixtures.length > 0}
										<Button size="sm" variant="ghost" icon="play" loading={starting === platform.id} disabled={platform.running} onclick={() => runOne(platform)}>Run</Button>
									{/if}
								</td>
							</tr>
							{#if expanded}
								<tr class="detail" id={`platform-${platform.id}`}>
									<td></td>
									<td colspan="10">
										<div class="detail-body">
											<dl class="kv">
												<dt>Hosts</dt>
												<dd><div class="chips">{#each platform.hosts as host (host)}<span class="chip">{host}</span>{/each}</div></dd>
												<dt>Media</dt>
												<dd>
													{#if platform.media.length}
														<div class="chips">{#each platform.media as m (m)}<span class="chip media" title={MEDIA_HINTS[m]}>{MEDIA_LABELS[m]}</span>{/each}</div>
													{:else}<span class="faint">Only hands links on to other platforms' resolvers.</span>{/if}
												</dd>
												<dt>Tags</dt>
												<dd>
													{#if platform.tags.length}
														<div class="chips">{#each platform.tags as t (t)}<span class="chip media" title={TAG_LABELS[t].hint}>{TAG_LABELS[t].label}</span>{/each}</div>
													{:else}<span class="faint">Not available</span>{/if}
												</dd>
												<dt>Features</dt>
												<dd>
													{#if platform.features.length}
														<div class="chips">{#each platform.features as feature (feature)}<span class="chip">{feature}</span>{/each}</div>
													{:else}<span class="faint">Not available</span>{/if}
												</dd>
												<dt>Formats</dt>
												<dd>
													{#if platform.formats.length}
														<div class="chips">{#each platform.formats as format (format)}<span class="chip">{format}</span>{/each}</div>
													{:else}<span class="faint">Not available</span>{/if}
												</dd>
												{#if platform.last_fail_at}
													<dt>Last failure</dt>
													<dd><Time value={platform.last_fail_at} /></dd>
												{/if}
											</dl>
											<div class="session">
												<div class="session-text">
													<span class="strong">Session</span>
													<span class="muted">{SESSION_LABELS[platform.session].label}. {SESSION_LABELS[platform.session].hint}</span>
													{#if platform.session !== 'none'}
														<span class="row">
															{@render sessionBadge(platform)}
															<span class="faint small">
																{#if platform.cookies > 0}{pluralize(platform.cookies, 'cookie')} stored <Time value={platform.cookies_updated_at} />{:else}no cookies stored{/if}{#if platform.session_check} · checked <Time value={platform.session_check.at} />{/if}
															</span>
														</span>
													{/if}
												</div>
												{#if canManageSessions && platform.session !== 'none'}
													<div class="row">
														<Button size="sm" icon="key" onclick={() => openImport(platform)}>Import cookies</Button>
														<Button size="sm" variant="ghost" icon="refresh" loading={checking === platform.id} onclick={() => checkSession(platform)}>Check session</Button>
														{#if platform.cookies > 0}
															<Button size="sm" variant="danger-soft" icon="trash" loading={clearing === platform.id} onclick={() => clearCookies(platform)}>Clear</Button>
														{/if}
													</div>
												{/if}
											</div>
											{#if platform.fixtures.length === 0}
												<p class="faint small">This platform names no fixture links.</p>
											{:else}
												<table class="table fixtures">
													<thead>
														<tr><th>Link</th><th>Status</th><th>Last run</th><th>Last pass</th><th>Took</th><th>Found</th></tr>
													</thead>
													<tbody>
														{#each platform.fixtures as fixture (fixture.url)}
															<tr>
																<td><a href={fixture.url} target="_blank" rel="noreferrer" class="break">{fixture.url}</a></td>
																<td>
																	{#if platform.running}
																		<Badge tone="info" size="sm" dot pulse>Running</Badge>
																	{:else}
																		<Badge tone={fixtureTone(fixture.status)} size="sm" dot={fixture.status !== 'never'}>{FIXTURE_STATUS_LABELS[fixture.status]}</Badge>
																	{/if}
																</td>
																<td><Time value={fixture.run_at} empty="never" /></td>
																<td><Time value={fixture.last_pass_at} empty="never" /></td>
																<td class="num">{duration(fixture.duration_ms)}</td>
																<td class="found">
																	{#if fixture.status === 'fail' && fixture.error}
																		<span class="error-text truncate-2" title={fixture.error}>{fixture.error}</span>
																	{:else if fixture.status === 'login_required' && fixture.error}
																		<span class="muted truncate-2" title={fixture.error}>{fixture.error}</span>
																	{:else if fixture.title || fixture.found}
																		{#if fixture.found}<span class="chip media">{describeFound(fixture.found)}</span>{/if}
																		{#if fixture.title}<span class="truncate-2" title={fixture.title}>{fixture.title}</span>{/if}
																	{:else}
																		<span class="faint">Not available</span>
																	{/if}
																</td>
															</tr>
														{/each}
													</tbody>
												</table>
											{/if}
										</div>
									</td>
								</tr>
							{/if}
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</div>
{/if}

<Dialog open={importFor !== null} title={importFor ? `Import cookies for ${importFor.name}` : 'Import cookies'} description="Use browser cookies to access media that requires login." busy={importing} onclose={() => (importFor = null)}>
	<form id="cookies-form" class="stack" onsubmit={runImport} novalidate>
		{#if importError}
			<FormFeedback message={importError} />
		{/if}
		<Field label="Format" for="cookies-format" hint={importFormat === 'netscape' ? 'A cookies.txt as browser extensions and yt-dlp export it.' : 'Copy a Cookie request header from your browser developer tools.'}>
			<select id="cookies-format" class="select" bind:value={importFormat} disabled={importing}>
				<option value="netscape">Netscape cookies.txt</option>
				<option value="header">Cookie header</option>
			</select>
		</Field>
		{#if importFormat === 'header'}
			<Field label="Domain" for="cookies-domain" hint="Includes subdomains.">
				<input id="cookies-domain" class="input mono" bind:value={importDomain} placeholder="example.com" autocomplete="off" spellcheck="false" disabled={importing} />
			</Field>
		{/if}
		<Field label="Cookies" for="cookies-text">
			<textarea id="cookies-text" class="textarea mono" rows="10" bind:value={importText} placeholder={importFormat === 'netscape' ? '# Netscape HTTP Cookie File\n.example.com\tTRUE\t/\tTRUE\t0\tname\tvalue' : 'name=value; other=value'} spellcheck="false" disabled={importing}></textarea>
		</Field>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (importFor = null)} disabled={importing}>Cancel</Button>
		<Button variant="primary" loading={importing} disabled={!importText.trim()} onclick={() => document.querySelector<HTMLFormElement>('#cookies-form')?.requestSubmit()}>Store and check</Button>
	{/snippet}
</Dialog>

<style>
	.session {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 12px;
		flex-wrap: wrap;
		padding: 12px 14px;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		background: var(--surface);
	}

	.session-text {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}

	.summary {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
		gap: 12px;
	}

	.stat {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 14px 16px;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		box-shadow: var(--shadow-sm);
	}

	.stat-value {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
		font-variant-numeric: tabular-nums;
	}

	.stat-value.ok {
		color: var(--ok-text);
	}

	.stat-value.danger {
		color: var(--danger-text);
	}

	.stat-value.warn {
		color: var(--warn-text);
	}

	.small-value {
		font-size: 15px;
	}

	.stat-label {
		font-size: 13px;
		color: var(--text-3);
	}

	.search {
		max-width: 380px;
	}

	.kinds {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
	}

	.chip.media {
		text-transform: none;
	}

	td.found {
		display: flex;
		flex-direction: column;
		gap: 4px;
		align-items: flex-start;
	}

	.toggle {
		width: 32px;
		padding-right: 0;
	}

	.expander {
		display: flex;
		padding: 4px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: var(--text-3);
		cursor: pointer;
	}

	.expander:hover {
		background: var(--surface-3);
		color: var(--text);
	}

	tr.expanded td {
		border-bottom: none;
	}

	tr.detail td {
		background: var(--surface-2);
	}

	tr.detail:hover td {
		background: var(--surface-2);
	}

	.detail-body {
		display: flex;
		flex-direction: column;
		gap: 14px;
		padding: 4px 0 8px;
	}

	.fixtures {
		font-size: 13px;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		background: var(--surface);
	}

	.fixtures th,
	.fixtures td {
		padding: 8px 12px;
	}

	td.found {
		max-width: 360px;
	}

	.truncate-2 {
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
		overflow-wrap: anywhere;
	}
</style>
