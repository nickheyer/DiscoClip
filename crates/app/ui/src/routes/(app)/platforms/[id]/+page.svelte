<script lang="ts">
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import PlayIcon from '@lucide/svelte/icons/play';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { platforms as platformsApi } from '$lib/api/endpoints';
	import type { FixtureResult, PlatformCoverage, SessionSupport } from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CookiesDialog from '$lib/components/CookiesDialog.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { host, mediaLabel, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const POLL = 3_000;
	/** Said on a session button another session action is holding up. */
	const WAIT = 'Wait for the current session action to finish';
	/** What a platform asks of a login, as the Session card names it. */
	const LOGIN: Record<SessionSupport, string> = {
		none: 'None',
		optional: 'Optional',
		required: 'Required'
	};

	const id = $derived(page.params.id ?? '');

	let platform = $state<PlatformCoverage | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let checking = $state(false);
	let sessionPending = $state<'check' | 'clear' | null>(null);
	let cookiesOpen = $state(false);
	let confirmClear = $state(false);

	let requestId = 0;
	async function load(quiet = false) {
		const current = ++requestId;
		if (!quiet) {
			loading = true;
			error = null;
		}
		try {
			const loaded = await platformsApi.get(id);
			if (current !== requestId) return;
			platform = loaded;
		} catch (err) {
			if (current !== requestId) return;
			if (!quiet) error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	$effect(() => {
		void id;
		void load();
	});

	onMount(() => {
		const timer = setInterval(() => {
			if (platform?.running) void load(true);
		}, POLL);
		return () => clearInterval(timer);
	});

	async function check() {
		checking = true;
		try {
			platform = await platformsApi.check(id);
			notify.success('Checks started');
		} catch (err) {
			reportError(err, 'Could not start the checks');
		} finally {
			checking = false;
		}
	}

	async function checkSession() {
		sessionPending = 'check';
		try {
			platform = await platformsApi.checkSession(id);
			const result = platform.session_check;
			if (result?.state === 'logged_in') notify.success('Session is logged in', result.account);
			else if (result?.state === 'logged_out') notify.error('Session is logged out');
			else notify.info('This platform has no login to verify');
		} catch (err) {
			reportError(err, 'Could not verify the login');
		} finally {
			sessionPending = null;
		}
	}

	async function clearCookies() {
		sessionPending = 'clear';
		try {
			platform = await platformsApi.clearCookies(id);
			notify.success('Cookies removed');
		} finally {
			sessionPending = null;
		}
	}

	function found(fixture: FixtureResult): string {
		const f = fixture.found;
		if (!f) return '';
		return f.kind === 'playlist'
			? `Playlist · ${number(f.entries)} entries`
			: `${mediaLabel(f.media)} · ${number(f.variants)} variants`;
	}

	/** The link's path, which names a link whose title is unknown. */
	function pathOf(url: string): string {
		try {
			const parsed = new URL(url);
			return `${parsed.pathname}${parsed.search}${parsed.hash}`.replace(/^\//, '') || '/';
		} catch {
			return url;
		}
	}

	/** What went wrong, as a sentence. Results stored before the message dropped the link
	 * it belongs to still carry it, so it comes off here. */
	function failure(fixture: FixtureResult): string {
		const message = fixture.error ?? '';
		if (!message.startsWith(`${fixture.url} `)) return message;
		const rest = message.slice(fixture.url.length + 1).replace(/^is /, '');
		return rest ? rest.charAt(0).toUpperCase() + rest.slice(1) : message;
	}

	const columns: Column<FixtureResult>[] = [
		{ key: 'url', label: 'Link', cell: urlCell, class: 'max-w-md' },
		{ key: 'status', label: 'Result', cell: statusCell },
		{ key: 'found', label: 'Found', value: found },
		{
			key: 'duration',
			label: 'Took',
			align: 'right',
			value: (f) => (f.duration_ms === null ? null : `${number(f.duration_ms)} ms`)
		},
		{ key: 'run', label: 'Run', cell: runCell }
	];

	const canCheck = $derived(session.can('manage_jobs'));
	const canSession = $derived(session.can('manage_settings'));
	const running = $derived(checking || platform?.running === true);

	/** The tags worth a badge: one that repeats a media kind says nothing the kind has not. */
	const tags = $derived.by(() => {
		if (!platform) return [];
		const kinds = new Set(platform.media.map((kind) => mediaLabel(kind).toLowerCase()));
		return platform.tags.filter((tag) => !kinds.has(tag.toLowerCase()));
	});

	/** How the last run went, link by link. */
	const links = $derived.by(() => {
		if (!platform) return '';
		let text = `${number(platform.passed)} of ${number(platform.fixtures.length)} pass`;
		if (platform.failed > 0) text += ` · ${number(platform.failed)} fail`;
		if (platform.login_required > 0) text += ` · ${number(platform.login_required)} need login`;
		return text;
	});

	const back = { href: resolve('/platforms'), label: 'Platforms' };
</script>

{#snippet urlCell(fixture: FixtureResult)}
	<div class="min-w-0">
		<a
			href={fixture.url}
			class="block truncate anchor font-medium"
			target="_blank"
			rel="noreferrer"
			title={fixture.url}
		>
			{fixture.title ?? pathOf(fixture.url)}
			<ExternalLinkIcon class="inline size-3" />
		</a>
		<p class="truncate text-sm text-surface-600-400">{host(fixture.url)}</p>
		{#if fixture.error}
			<p class="mt-1 text-sm break-words text-error-600-400">{failure(fixture)}</p>
		{/if}
	</div>
{/snippet}
{#snippet statusCell(fixture: FixtureResult)}
	<Status fixture={fixture.status} />
{/snippet}
{#snippet runCell(fixture: FixtureResult)}
	<div class="whitespace-nowrap">
		<RelativeTime at={fixture.run_at} />
		{#if fixture.status === 'fail' || fixture.status === 'login_required'}
			<p class="text-sm text-surface-600-400">
				Last pass: <RelativeTime at={fixture.last_pass_at} />
			</p>
		{/if}
	</div>
{/snippet}

{#if error && !loading}
	<PageHeader title="Platform" {back} />
	<ErrorState {error} title="This platform could not be loaded" onretry={() => load()} />
{:else if !platform}
	<PageHeader title="Platform" {back} />
	<div class="space-y-3" aria-busy="true">
		<div class="h-10 placeholder w-1/2 animate-pulse"></div>
		<div class="h-48 placeholder animate-pulse"></div>
	</div>
{:else}
	<PageHeader title={platform.name} {back}>
		<div class="flex flex-wrap items-center gap-1.5">
			<span class="badge preset-tonal font-mono" style="--badge-size: var(--text-xs)">
				{platform.id}
			</span>
			{#each platform.media as kind (kind)}
				<span class="badge preset-tonal" style="--badge-size: var(--text-xs)">
					<MediaKindIcon {kind} />
					{mediaLabel(kind)}
				</span>
			{/each}
			{#each tags as tag (tag)}
				<span class="badge preset-outlined-surface-300-700" style="--badge-size: var(--text-xs)">
					{tag}
				</span>
			{/each}
		</div>
		{#snippet actions()}
			{#if canCheck}
				<button
					type="button"
					class="btn preset-filled-primary-500"
					onclick={check}
					disabled={running}
					aria-busy={running}
				>
					{#if running}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
					{running ? 'Running checks…' : 'Run checks'}
				</button>
			{/if}
		{/snippet}
	</PageHeader>

	<div class="grid items-start gap-6 lg:grid-cols-2">
		<Card title="Capabilities">
			<KeyValue>
				<KeyValueRow label="Hosts">
					<span class="font-mono text-xs break-all">{platform.hosts.join(', ')}</span>
				</KeyValueRow>
				<KeyValueRow
					label="Features"
					value={platform.features.length > 0 ? platform.features.join(', ') : null}
				/>
				<KeyValueRow
					label="Formats"
					value={platform.formats.length > 0 ? platform.formats.join(', ') : null}
				/>
			</KeyValue>
		</Card>

		<Card title="Session">
			<div class="space-y-4">
				<KeyValue>
					<KeyValueRow label="Login" value={LOGIN[platform.session]} />
					<KeyValueRow
						label="Cookies"
						value={platform.cookies === 0 ? null : `${number(platform.cookies)} saved`}
					/>
					<KeyValueRow label="Updated">
						<RelativeTime at={platform.cookies_updated_at} />
					</KeyValueRow>
					<KeyValueRow label="Last verified">
						{#if sessionPending === 'check'}
							<span class="inline-flex items-center gap-2"><Spinner />Verifying now</span>
						{:else if platform.session_check}
							<span class="inline-flex flex-wrap items-center gap-2">
								<Status session={platform.session_check.state} />
								<RelativeTime at={platform.session_check.at} class="text-surface-600-400" />
							</span>
						{:else}
							<RelativeTime at={null} />
						{/if}
					</KeyValueRow>
					{#if platform.session_check?.state === 'logged_in'}
						<KeyValueRow label="Account" value={platform.session_check.account} />
					{/if}
				</KeyValue>
				{#if canSession && platform.session !== 'none'}
					<div class="flex flex-wrap gap-2">
						<button type="button" class="btn preset-filled" onclick={() => (cookiesOpen = true)}>
							Import cookies
						</button>
						<button
							type="button"
							class="btn preset-tonal"
							onclick={checkSession}
							disabled={sessionPending !== null || platform.cookies === 0}
							aria-busy={sessionPending === 'check'}
							title={sessionPending === 'clear' ? WAIT : undefined}
						>
							{#if sessionPending === 'check'}<Spinner />{/if}
							{sessionPending === 'check' ? 'Verifying…' : 'Verify login'}
						</button>
						<button
							type="button"
							class="btn preset-tonal-error"
							onclick={() => (confirmClear = true)}
							disabled={sessionPending !== null || platform.cookies === 0}
							aria-busy={sessionPending === 'clear'}
							title={sessionPending === 'check' ? WAIT : undefined}
						>
							{#if sessionPending === 'clear'}<Spinner />{/if}
							{sessionPending === 'clear' ? 'Clearing…' : 'Clear cookies'}
						</button>
					</div>
				{/if}
			</div>
		</Card>
	</div>

	<Card title="Link checks" count={links} flush>
		{#snippet actions()}
			{#if platform}
				<span class="text-sm text-surface-600-400">
					Run <RelativeTime at={platform.last_run_at} />
					<span title="The most recent run in which every link passed">
						· full pass <RelativeTime at={platform.last_pass_at} />
					</span>
					· fail <RelativeTime at={platform.last_fail_at} />
				</span>
			{/if}
		{/snippet}
		<DataTable rows={platform.fixtures} {columns} rowKey={(f) => f.url} flush class="p-2">
			{#snippet empty()}
				This platform ships no check links.
			{/snippet}
		</DataTable>
	</Card>

	<CookiesDialog
		bind:open={cookiesOpen}
		{platform}
		onimported={(outcome) => (platform = outcome)}
	/>
{/if}

<Confirm
	bind:open={confirmClear}
	title="Remove the saved cookies?"
	message="Links needing a login stop resolving until new cookies are imported."
	confirmLabel="Remove"
	danger
	onconfirm={clearCookies}
/>
