<script lang="ts">
	import ExternalLinkIcon from '@lucide/svelte/icons/external-link';
	import PencilIcon from '@lucide/svelte/icons/pencil';
	import PlayIcon from '@lucide/svelte/icons/play';
	import PlusIcon from '@lucide/svelte/icons/plus';
	import Trash2Icon from '@lucide/svelte/icons/trash-2';
	import { Switch } from '@skeletonlabs/skeleton-svelte';
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { platforms as platformsApi } from '$lib/api/endpoints';
	import type {
		FixtureResult,
		LinkOrigin,
		PlatformCoverage,
		SessionSupport,
		Uuid
	} from '$lib/api/types';
	import Card from '$lib/components/Card.svelte';
	import Confirm from '$lib/components/Confirm.svelte';
	import CookiesDialog from '$lib/components/CookiesDialog.svelte';
	import DataTable, { type Column } from '$lib/components/DataTable.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import Field from '$lib/components/Field.svelte';
	import KeyValue from '$lib/components/KeyValue.svelte';
	import KeyValueRow from '$lib/components/KeyValueRow.svelte';
	import MediaKindIcon from '$lib/components/MediaKindIcon.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import ChipList from '$lib/components/ChipList.svelte';
	import Count from '$lib/components/Count.svelte';
	import Duration from '$lib/components/Duration.svelte';
	import { HEALTH, proofOf } from '$lib/components/PlatformSummary.svelte';
	import Timestamp from '$lib/components/Timestamp.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Status from '$lib/components/Status.svelte';
	import { host, mediaLabel, number } from '$lib/format';
	import { session } from '$lib/session.svelte';
	import { notify, reportError } from '$lib/toast.svelte';

	const POLL = 3_000;
	/** Said on a session button another session action is holding up. */
	const WAIT = 'Wait for the current session action to finish';
	/** What a platform asks of a login, as the Session card names it. */
	const LOGIN: Record<Exclude<SessionSupport, 'none'>, string> = {
		optional: 'Optional',
		required: 'Required'
	};
	/** Where a link came from, as its badge says. */
	const ORIGIN: Record<LinkOrigin, string> = {
		builtin: 'Built in',
		custom: 'Added',
		job: 'From a job'
	};

	const id = $derived(page.params.id ?? '');

	let platform = $state<PlatformCoverage | null>(null);
	let loading = $state(true);
	let error = $state<unknown>(null);
	let checking = $state(false);
	let sessionPending = $state<'check' | 'clear' | null>(null);
	let cookiesOpen = $state(false);
	let confirmClear = $state(false);

	/** The links a request is in flight for. */
	const linkPending = new SvelteSet<Uuid>();
	let linkOpen = $state(false);
	/** The link the dialog edits, or nothing while it adds one. */
	let editing = $state<FixtureResult | null>(null);
	let linkUrl = $state('');
	let savingLink = $state(false);
	let removing = $state<FixtureResult | null>(null);
	let confirmRemove = $state(false);

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
			notify.success('Check started');
		} catch (err) {
			reportError(err, 'Could not start the check');
		} finally {
			checking = false;
		}
	}

	async function checkLink(link: FixtureResult) {
		linkPending.add(link.id);
		try {
			platform = await platformsApi.checkLink(id, link.id);
			notify.success('Link check started');
		} catch (err) {
			reportError(err, 'Could not start the link check');
		} finally {
			linkPending.delete(link.id);
		}
	}

	async function toggleLink(link: FixtureResult, enabled: boolean) {
		linkPending.add(link.id);
		try {
			platform = await platformsApi.changeLink(id, link.id, { enabled });
			notify.success(enabled ? 'Link in use' : 'Link set aside');
		} catch (err) {
			reportError(err, 'Could not switch the link');
		} finally {
			linkPending.delete(link.id);
		}
	}

	function openAdd() {
		editing = null;
		linkUrl = '';
		linkOpen = true;
	}

	function openEdit(link: FixtureResult) {
		editing = link;
		linkUrl = link.url;
		linkOpen = true;
	}

	async function saveLink(event: SubmitEvent) {
		event.preventDefault();
		savingLink = true;
		try {
			const url = linkUrl.trim();
			platform = editing
				? await platformsApi.changeLink(id, editing.id, { url })
				: await platformsApi.addLink(id, { url });
			notify.success(editing ? 'Link changed' : 'Link added');
			linkOpen = false;
		} catch (err) {
			reportError(err, editing ? 'Could not change the link' : 'Could not add the link');
		} finally {
			savingLink = false;
		}
	}

	async function removeLink() {
		if (!removing) return;
		platform = await platformsApi.removeLink(id, removing.id);
		notify.success('Link removed');
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

	const canCheck = $derived(session.can('manage_jobs'));
	const canSession = $derived(session.can('manage_settings'));
	const running = $derived(checking || platform?.running === true);

	const columns = $derived.by((): Column<FixtureResult>[] => {
		const shown: Column<FixtureResult>[] = [
			{ key: 'url', label: 'Link', cell: urlCell, class: 'max-w-md' },
			{ key: 'status', label: 'Result', cell: statusCell },
			{ key: 'found', label: 'Found', cell: foundCell },
			{ key: 'duration', label: 'Took', align: 'right', cell: tookCell },
			{ key: 'run', label: 'Run', cell: runCell },
			{ key: 'enabled', label: 'In use', cell: enabledCell }
		];
		if (canCheck) {
			shown.push({
				key: 'actions',
				label: 'Actions',
				hideLabel: true,
				cell: actionsCell,
				align: 'right'
			});
		}
		return shown;
	});

	/** The tags worth a badge: one that repeats a media kind says nothing the kind has not. */
	const tags = $derived.by(() => {
		if (!platform) return [];
		const kinds = new Set(platform.media.map((kind) => mediaLabel(kind).toLowerCase()));
		return platform.tags.filter((tag) => !kinds.has(tag.toLowerCase()));
	});

	const health = $derived(
		platform?.running
			? { label: 'Checking', tone: 'primary' as const }
			: HEALTH[platform?.health ?? 'unknown']
	);
	const proof = $derived(platform ? proofOf(platform) : null);
	const inUse = $derived(platform?.fixtures.filter((f) => f.enabled).length ?? 0);

	const back = { href: resolve('/platforms'), label: 'Platforms' };
</script>

{#snippet urlCell(fixture: FixtureResult)}
	<div class="min-w-0">
		<p class="flex min-w-0 items-center gap-2">
			<a
				href={fixture.url}
				class="truncate anchor font-medium"
				target="_blank"
				rel="noreferrer"
				title={fixture.url}
			>
				{fixture.title ?? pathOf(fixture.url)}
				<ExternalLinkIcon class="inline size-3" />
			</a>
			<span
				class="badge shrink-0 preset-outlined-surface-300-700"
				style="--badge-size: var(--text-xs)"
			>
				{ORIGIN[fixture.origin]}
			</span>
		</p>
		<p class="truncate text-sm text-surface-600-400">{host(fixture.url)}</p>
		{#if fixture.error}
			<p class="mt-1 text-sm break-words text-error-600-400">{failure(fixture)}</p>
		{/if}
	</div>
{/snippet}
{#snippet statusCell(fixture: FixtureResult)}
	<Status fixture={fixture.status} />
{/snippet}
{#snippet foundCell(fixture: FixtureResult)}
	{#if fixture.found}
		{#if fixture.found.kind === 'playlist'}
			Playlist · <Count value={fixture.found.entries} noun="entry" plural="entries" />
		{:else}
			{mediaLabel(fixture.found.media)} · <Count value={fixture.found.variants} noun="variant" />
		{/if}
	{/if}
{/snippet}
{#snippet tookCell(fixture: FixtureResult)}
	{#if fixture.duration_ms !== null}
		<Duration value={fixture.duration_ms / 1000} />
	{/if}
{/snippet}
{#snippet runCell(fixture: FixtureResult)}
	<div class="whitespace-nowrap">
		<Timestamp at={fixture.run_at} />
		{#if fixture.status === 'fail' || fixture.status === 'login_required'}
			<p class="text-sm text-surface-600-400">
				Last pass: <Timestamp at={fixture.last_pass_at} />
			</p>
		{/if}
	</div>
{/snippet}
<!-- A link set aside on its own, for failing while another resolved, says why. -->
{#snippet enabledCell(fixture: FixtureResult)}
	{@const busy = linkPending.has(fixture.id)}
	<div class="flex items-center gap-2">
		{#if canCheck}
			<Switch
				checked={fixture.enabled}
				disabled={busy}
				onCheckedChange={(details) => toggleLink(fixture, details.checked)}
			>
				<Switch.Control><Switch.Thumb /></Switch.Control>
				<Switch.Label class="sr-only">Use {fixture.url} in checks</Switch.Label>
				<Switch.HiddenInput />
			</Switch>
		{:else}
			<Status enabled={fixture.enabled} />
		{/if}
		{#if !fixture.enabled && fixture.disabled_reason}
			<span class="text-sm text-surface-600-400" title={fixture.disabled_reason}> Set aside </span>
		{/if}
	</div>
{/snippet}
{#snippet actionsCell(fixture: FixtureResult)}
	{@const busy = linkPending.has(fixture.id) || running}
	<div class="flex justify-end gap-1">
		<button
			type="button"
			class="btn-icon btn-icon-sm hover:preset-tonal"
			onclick={() => checkLink(fixture)}
			disabled={busy}
			title="Run this link"
			aria-label="Run {fixture.url}"
		>
			{#if linkPending.has(fixture.id)}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
		</button>
		<button
			type="button"
			class="btn-icon btn-icon-sm hover:preset-tonal"
			onclick={() => openEdit(fixture)}
			disabled={busy}
			title="Edit the link"
			aria-label="Edit {fixture.url}"
		>
			<PencilIcon class="size-4" />
		</button>
		<button
			type="button"
			class="btn-icon btn-icon-sm hover:preset-tonal-error"
			onclick={() => {
				removing = fixture;
				confirmRemove = true;
			}}
			disabled={busy}
			title="Remove the link"
			aria-label="Remove {fixture.url}"
		>
			<Trash2Icon class="size-4" />
		</button>
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
		<!-- The verdict, what proves it, and when the links were last tried. -->
		<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-surface-600-400">
			<Status label={health.label} tone={health.tone} pulse={platform.running} />
			{#if proof?.by === 'job'}
				<span>job finished <Timestamp at={proof.at} /></span>
			{:else if proof?.by === 'link'}
				<span>link resolved <Timestamp at={proof.at} /></span>
			{/if}
			{#if platform.last_run_at}
				<span>checked <Timestamp at={platform.last_run_at} /></span>
			{/if}
		</div>
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
			{#if !platform.on_by_default}
				<span
					class="badge preset-tonal-warning"
					style="--badge-size: var(--text-xs)"
					title="Profiles leave this platform off until an exception or a chosen preset names it"
				>
					Off until turned on
				</span>
			{/if}
		</div>
		{#snippet actions()}
			{#if canCheck}
				<button
					type="button"
					class="btn preset-filled-primary-500"
					onclick={check}
					disabled={running || inUse === 0}
					aria-busy={running}
				>
					{#if running}<Spinner />{:else}<PlayIcon class="size-4" />{/if}
					{running ? 'Checking…' : 'Check now'}
				</button>
			{/if}
		{/snippet}
	</PageHeader>

	<div class="grid items-start gap-6 lg:grid-cols-2">
		<Card title="Capabilities">
			<KeyValue>
				<KeyValueRow label="Hosts">
					<ChipList items={platform.hosts} mono />
				</KeyValueRow>
				{#if platform.features.length > 0}
					<KeyValueRow label="Features"><ChipList items={platform.features} /></KeyValueRow>
				{/if}
				{#if platform.formats.length > 0}
					<KeyValueRow label="Formats"><ChipList items={platform.formats} /></KeyValueRow>
				{/if}
				{#if platform.last_job_at}
					<KeyValueRow label="Last job"><Timestamp at={platform.last_job_at} /></KeyValueRow>
				{/if}
			</KeyValue>
		</Card>

		{#if platform.session !== 'none'}
			<Card title="Session">
				<div class="space-y-4">
					<KeyValue>
						<KeyValueRow label="Login" value={LOGIN[platform.session]} />
						<KeyValueRow
							label="Cookies"
							value={platform.cookies === 0 ? null : `${number(platform.cookies)} saved`}
						/>
						{#if platform.cookies_updated_at}
							<KeyValueRow label="Updated">
								<Timestamp at={platform.cookies_updated_at} />
							</KeyValueRow>
						{/if}
						{#if sessionPending === 'check' || platform.session_check}
							<KeyValueRow label="Last verified">
								{#if sessionPending === 'check'}
									<span class="inline-flex items-center gap-2"><Spinner />Verifying now</span>
								{:else if platform.session_check}
									<span class="inline-flex flex-wrap items-center gap-2">
										<Status session={platform.session_check.state} />
										<Timestamp at={platform.session_check.at} class="text-surface-600-400" />
									</span>
								{/if}
							</KeyValueRow>
						{/if}
						{#if platform.session_check?.state === 'logged_in'}
							<KeyValueRow label="Account" value={platform.session_check.account} />
						{/if}
					</KeyValue>
					{#if canSession}
						<div class="flex flex-wrap gap-2">
							<button type="button" class="btn preset-filled" onclick={() => (cookiesOpen = true)}>
								Import cookies
							</button>
							<button
								type="button"
								class="btn preset-tonal"
								onclick={async () => {
									sessionPending = 'check';
									try {
										platform = await platformsApi.checkSession(id);
										const result = platform.session_check;
										if (result?.state === 'logged_in')
											notify.success('Session is logged in', result.account);
										else if (result?.state === 'logged_out') notify.error('Session is logged out');
										else notify.info('This platform has no login to verify');
									} catch (err) {
										reportError(err, 'Could not verify the login');
									} finally {
										sessionPending = null;
									}
								}}
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
		{/if}
	</div>

	<!-- The links the platform is checked with. A check tries them in turn, the one that
	     resolved most recently first, and stops at the first that resolves. -->
	<Card title="Check links" count="{number(inUse)} in use" flush>
		{#snippet actions()}
			{#if canCheck}
				<button type="button" class="btn preset-tonal btn-sm" onclick={openAdd}>
					<PlusIcon class="size-4" />
					Add link
				</button>
			{/if}
		{/snippet}
		<DataTable rows={platform.fixtures} {columns} rowKey={(f) => f.id} flush class="p-2">
			{#snippet empty()}
				No check links. Finished jobs add their links here, or add one.
			{/snippet}
		</DataTable>
	</Card>

	<CookiesDialog
		bind:open={cookiesOpen}
		{platform}
		onimported={(outcome) => (platform = outcome)}
	/>
{/if}

<Modal
	bind:open={linkOpen}
	title={editing ? 'Edit the check link' : 'Add a check link'}
	description="A public link on this platform that should resolve to media."
	busy={savingLink}
>
	<form id="check-link" class="space-y-4" onsubmit={saveLink}>
		<Field label="Link" for="check-link-url" required>
			<input
				id="check-link-url"
				class="input font-mono"
				type="url"
				bind:value={linkUrl}
				required
				autocomplete="off"
			/>
		</Field>
	</form>
	{#snippet footer()}
		<button
			type="button"
			class="btn preset-tonal"
			onclick={() => (linkOpen = false)}
			disabled={savingLink}>Cancel</button
		>
		<button
			type="submit"
			form="check-link"
			class="btn preset-filled-primary-500"
			disabled={savingLink}
		>
			{#if savingLink}<Spinner />{/if}
			{editing ? 'Save' : 'Add'}
		</button>
	{/snippet}
</Modal>

<Confirm
	bind:open={confirmRemove}
	title="Remove this check link?"
	message={removing?.url ?? ''}
	confirmLabel="Remove"
	danger
	onconfirm={removeLink}
/>

<Confirm
	bind:open={confirmClear}
	title="Remove the saved cookies?"
	message="Links needing a login stop resolving until new cookies are imported."
	confirmLabel="Remove"
	danger
	onconfirm={async () => {
		sessionPending = 'clear';
		try {
			platform = await platformsApi.clearCookies(id);
			notify.success('Cookies removed');
		} finally {
			sessionPending = null;
		}
	}}
/>
