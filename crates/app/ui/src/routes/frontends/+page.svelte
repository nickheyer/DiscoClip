<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { SECRET_KINDS, frontends as api, messageOf } from '$lib/api';
	import type { Frontend, FrontendInput, FrontendUser, ViewerSession } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import TagInput from '$lib/components/TagInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { describeUserAgent, formatBytes, isSnowflake, pluralize } from '$lib/format';
	import {
		SECRET_LABELS,
		accessSummary,
		emptyFrontend,
		linkSummary,
		scopeSummary,
		slugProblem,
		toInput
	} from '$lib/frontends';
	import { confirm } from '$lib/state/confirm.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const refresh = () => invalidate('app:frontends');
	const defaultProfile = $derived(
		data.profiles.find((p) => p.builtin)?.id ?? data.profiles[0]?.id ?? ''
	);
	const profileName = (id: string) => data.profiles.find((p) => p.id === id)?.name ?? id;
	const snowflake = (what: string) => (value: string) =>
		isSnowflake(value) ? null : `${value} is not a Discord ${what} id`;

	// Add and edit

	let dialog = $state(false);
	let editing = $state<Frontend | null>(null);
	let form = $state<FrontendInput>(emptyFrontend(''));
	let saving = $state(false);
	let error = $state<string | null>(null);
	let minBitrateKb = $state(1500);
	let maxBytesMb = $state(2048);
	let secret = $state('');

	function openAdd() {
		editing = null;
		form = emptyFrontend(defaultProfile);
		minBitrateKb = Math.round(form.links.min_bitrate / 1000);
		maxBytesMb = Math.round(form.links.max_bytes / (1024 * 1024));
		error = null;
		dialog = true;
	}

	function openEdit(frontend: Frontend) {
		editing = frontend;
		form = toInput(frontend);
		minBitrateKb = Math.round(form.links.min_bitrate / 1000);
		maxBytesMb = Math.round(form.links.max_bytes / (1024 * 1024));
		error = null;
		dialog = true;
	}

	const nameProblem = $derived(form.name.trim() === '' ? 'A front end needs a name.' : null);
	const slugIssue = $derived(slugProblem(form.slug.trim()));
	const noWayIn = $derived(
		!form.access.open &&
			!form.access.accounts &&
			form.access.providers.length === 0 &&
			!(form.access.secret_kind && (editing?.has_secret || secret.trim() !== ''))
	);
	const discordListed = $derived(form.access.providers.includes('discord'));

	function toggleProvider(id: string, on: boolean) {
		const set = new Set(form.access.providers);
		if (on) set.add(id);
		else set.delete(id);
		form.access.providers = [...set];
		if (!set.has('discord')) {
			form.access.discord_members = false;
			form.access.discord_users = [];
		}
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (nameProblem || slugIssue) {
			error = nameProblem ?? slugIssue;
			return;
		}
		saving = true;
		error = null;
		const input: FrontendInput = {
			...form,
			name: form.name.trim(),
			slug: form.slug.trim(),
			description: form.description.trim(),
			links: {
				...form.links,
				min_bitrate: Math.max(1, Math.round(minBitrateKb)) * 1000,
				max_bytes: Math.max(1, Math.round(maxBytesMb)) * 1024 * 1024
			}
		};
		try {
			let saved: Frontend;
			if (editing) {
				saved = await api.update(editing.id, input);
			} else {
				saved = await api.create(input);
			}
			if (secret.trim() !== '') {
				await api.setSecret(saved.id, secret.trim());
				secret = '';
			}
			toast.ok(`Front end ${saved.name} ${editing ? 'saved' : 'added'}. It lives at /f/${saved.slug}.`);
			dialog = false;
			await refresh();
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	async function clearSecret() {
		if (!editing) return;
		const ok = await confirm.ask({
			title: `Remove the shared secret of ${editing.name}?`,
			message: 'Nobody gets in with it from then on; sessions already open stay open.',
			confirmLabel: 'Remove secret',
			danger: true
		});
		if (!ok) return;
		saving = true;
		try {
			editing = await api.setSecret(editing.id, null);
			toast.ok('Shared secret removed.');
			await refresh();
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	let deleting = $state<string | null>(null);

	async function remove(frontend: Frontend) {
		const ok = await confirm.ask({
			title: `Remove the front end ${frontend.name}?`,
			message: `/f/${frontend.slug} stops answering, its accounts and viewer sessions go, and the bots upload instead of linking to it.`,
			confirmLabel: 'Remove front end',
			danger: true
		});
		if (!ok) return;
		deleting = frontend.id;
		try {
			await api.remove(frontend.id);
			toast.ok(`Front end ${frontend.name} removed.`);
			if (detail?.id === frontend.id) detail = null;
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the front end: ${messageOf(cause)}`);
		} finally {
			deleting = null;
		}
	}

	// Accounts and sessions of one front end

	let detail = $state<Frontend | null>(null);
	let users = $state<FrontendUser[]>([]);
	let sessions = $state<ViewerSession[]>([]);
	let detailError = $state<string | null>(null);
	let loadingDetail = $state(false);

	async function openDetail(frontend: Frontend) {
		detail = frontend;
		detailError = null;
		loadingDetail = true;
		try {
			[users, sessions] = await Promise.all([api.users(frontend.id), api.sessions(frontend.id)]);
		} catch (cause) {
			detailError = messageOf(cause);
		} finally {
			loadingDetail = false;
		}
	}

	async function reloadDetail() {
		if (detail) await openDetail(detail);
	}

	let newUsername = $state('');
	let newPassword = $state('');
	let addingUser = $state(false);

	async function addUser(event: SubmitEvent) {
		event.preventDefault();
		if (!detail || newUsername.trim() === '' || newPassword === '') return;
		addingUser = true;
		detailError = null;
		try {
			await api.createUser(detail.id, newUsername.trim(), newPassword);
			toast.ok(`Account ${newUsername.trim()} added to ${detail.name}.`);
			newUsername = '';
			newPassword = '';
			await reloadDetail();
		} catch (cause) {
			detailError = messageOf(cause);
		} finally {
			addingUser = false;
		}
	}

	let resetting = $state<FrontendUser | null>(null);
	let resetPassword = $state('');
	let savingReset = $state(false);
	let resetError = $state<string | null>(null);

	async function saveReset(event: SubmitEvent) {
		event.preventDefault();
		if (!detail || !resetting || resetPassword === '') return;
		savingReset = true;
		resetError = null;
		try {
			await api.setUserPassword(detail.id, resetting.id, resetPassword);
			toast.ok(`Password of ${resetting.username} reset.`);
			resetting = null;
			resetPassword = '';
		} catch (cause) {
			resetError = messageOf(cause);
		} finally {
			savingReset = false;
		}
	}

	async function removeUser(user: FrontendUser) {
		if (!detail) return;
		const ok = await confirm.ask({
			title: `Remove the account ${user.username}?`,
			message: 'Its sessions end at once.',
			confirmLabel: 'Remove account',
			danger: true
		});
		if (!ok) return;
		try {
			await api.removeUser(detail.id, user.id);
			toast.ok(`Account ${user.username} removed.`);
			await reloadDetail();
		} catch (cause) {
			toast.error(`Could not remove the account: ${messageOf(cause)}`);
		}
	}

	async function endSession(session: ViewerSession) {
		if (!detail) return;
		try {
			await api.revokeSession(detail.id, session.id);
			toast.ok(`Session of ${session.display} ended.`);
			await reloadDetail();
		} catch (cause) {
			toast.error(`Could not end the session: ${messageOf(cause)}`);
		}
	}

	async function endAllSessions() {
		if (!detail) return;
		const ok = await confirm.ask({
			title: `End every viewer session of ${detail.name}?`,
			message: 'Everyone logged into the front end has to log in again.',
			confirmLabel: 'End all sessions',
			danger: true
		});
		if (!ok) return;
		try {
			await api.revokeSessions(detail.id);
			toast.ok('Every viewer session ended.');
			await reloadDetail();
		} catch (cause) {
			toast.error(`Could not end the sessions: ${messageOf(cause)}`);
		}
	}

	function subjectLabel(subject: string): string {
		if (subject === 'secret') return 'shared secret';
		if (subject.startsWith('account:')) return `account ${subject.slice('account:'.length)}`;
		if (subject.startsWith('provider:')) {
			const [, provider] = subject.split(':');
			return `${provider} login`;
		}
		return subject;
	}
</script>

<svelte:head>
	<title>Front ends · DiscoClip</title>
</svelte:head>

<PageHeader
	title="Front ends"
	description="Public sites over the media the server has made: each shows a guild, a channel or everything, on the platforms a profile turns on, to whoever it lets in. A front end can also be where the bots send Discord instead of an upload."
>
	{#snippet actions()}
		<Button variant="primary" icon="plus" onclick={openAdd}>New front end</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-header">
			<div>
				<h2>Front ends</h2>
				<p class="hint">Each lives at /f/&lt;slug&gt; on this server.</p>
			</div>
			<span class="faint small">{pluralize(data.frontends.length, 'front end')}</span>
		</div>
		{#if data.frontends.length === 0}
			<div class="card-body"><Empty compact icon="monitor" title="No front ends" description="The server hosts no public site yet." /></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Front end</th>
							<th>Shows</th>
							<th>Lets in</th>
							<th>Discord gets</th>
							<th>Updated</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each data.frontends as frontend (frontend.id)}
							<tr>
								<td>
									<div class="stack-sm" style="gap:2px">
										<span class="row">
											<span class="strong">{frontend.name}</span>
											{#if !frontend.enabled}<Badge size="sm" tone="warn">Off</Badge>{/if}
										</span>
										<a class="small mono" href={`/f/${frontend.slug}`} target="_blank" rel="noreferrer">/f/{frontend.slug}</a>
										{#if frontend.description}<span class="muted small">{frontend.description}</span>{/if}
									</div>
								</td>
								<td class="small">{scopeSummary(frontend)}<span class="faint block">via {profileName(frontend.profile_id)}{#if !frontend.downloads} · no downloads{/if}</span></td>
								<td class="small">{accessSummary(frontend, data.providers)}</td>
								<td class="small">{linkSummary(frontend)}</td>
								<td><Time value={frontend.updated_at} /></td>
								<td class="actions">
									<Button size="sm" variant="ghost" icon="users" onclick={() => openDetail(frontend)}>Accounts</Button>
									<Button size="sm" variant="ghost" icon="pencil" onclick={() => openEdit(frontend)}>Edit</Button>
									<Button size="sm" variant="ghost" icon="trash" loading={deleting === frontend.id} title="Remove front end" onclick={() => remove(frontend)} square />
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	{#if detail}
		<section class="card">
			<div class="card-header">
				<div>
					<h2>{detail.name}: accounts and viewers</h2>
					<p class="hint">The front end's own accounts, and who is logged in right now.</p>
				</div>
				<Button size="sm" variant="ghost" icon="x" onclick={() => (detail = null)}>Close</Button>
			</div>
			<div class="card-body stack">
				{#if detailError}
					<Alert tone="danger" message={detailError} onclose={() => (detailError = null)} />
				{/if}
				<div class="grid-2">
					<div class="stack">
						<h3 class="sub">Accounts</h3>
						{#if !detail.access.accounts}
							<p class="muted small">Accounts are not a way into this front end; turn them on in its settings for these to log in.</p>
						{/if}
						{#if loadingDetail}
							<p class="faint small">Loading…</p>
						{:else if users.length === 0}
							<p class="faint small">No accounts.</p>
						{:else}
							<ul class="plain stack-sm">
								{#each users as user (user.id)}
									<li class="row-between">
										<span class="row"><span class="strong">{user.username}</span><span class="faint small">since <Time value={user.created_at} /></span></span>
										<span class="row">
											<Button size="sm" variant="ghost" icon="key" onclick={() => { resetting = user; resetPassword = ''; resetError = null; }}>Reset password</Button>
											<Button size="sm" variant="ghost" icon="trash" title="Remove account" onclick={() => removeUser(user)} square />
										</span>
									</li>
								{/each}
							</ul>
						{/if}
						<form class="row add" onsubmit={addUser}>
							<input class="input" placeholder="Username" bind:value={newUsername} maxlength="40" autocomplete="off" disabled={addingUser} aria-label="New account username" />
							<PasswordInput id="fe-new-password" bind:value={newPassword} placeholder="Password" autocomplete="new-password" disabled={addingUser} />
							<Button type="submit" variant="primary" size="sm" loading={addingUser} disabled={newUsername.trim() === '' || newPassword === ''}>Add account</Button>
						</form>
					</div>
					<div class="stack">
						<div class="row-between">
							<h3 class="sub">Viewer sessions</h3>
							{#if sessions.length}
								<Button size="sm" variant="danger-soft" onclick={endAllSessions}>End all</Button>
							{/if}
						</div>
						{#if loadingDetail}
							<p class="faint small">Loading…</p>
						{:else if sessions.length === 0}
							<p class="faint small">Nobody is logged in.</p>
						{:else}
							<ul class="plain stack-sm">
								{#each sessions as session (session.id)}
									<li class="row-between">
										<span class="stack-sm" style="gap:0">
											<span class="row"><span class="strong">{session.display}</span><span class="faint small">{subjectLabel(session.subject)}</span></span>
											<span class="faint small">{describeUserAgent(session.user_agent)}{#if session.ip} · {session.ip}{/if} · seen <Time value={session.last_seen_at} /></span>
										</span>
										<Button size="sm" variant="ghost" icon="x" title="End session" onclick={() => endSession(session)} square />
									</li>
								{/each}
							</ul>
						{/if}
					</div>
				</div>
			</div>
		</section>
	{/if}
</div>

<Dialog bind:open={dialog} title={editing ? `Edit front end ${editing.name}` : 'New front end'} size="lg" busy={saving}>
	<form id="frontend-form" class="stack" onsubmit={save} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<div class="grid-2">
			<Field label="Name" for="fe-name" error={error && nameProblem ? nameProblem : null}>
				<input id="fe-name" class="input" bind:value={form.name} maxlength="80" disabled={saving} autocomplete="off" />
			</Field>
			<Field label="Slug" for="fe-slug" hint={`The site lives at /f/${form.slug.trim() || '<slug>'}.`} error={form.slug.trim() !== '' ? slugIssue : null}>
				<input id="fe-slug" class="input mono" bind:value={form.slug} maxlength="40" disabled={saving} autocomplete="off" spellcheck="false" />
			</Field>
		</div>
		<Field label="Description" for="fe-description" optional hint="Shown under the name on the site.">
			<input id="fe-description" class="input" bind:value={form.description} maxlength="500" disabled={saving} autocomplete="off" />
		</Field>
		<div class="grid-2">
			<label class="checkbox">
				<input type="checkbox" bind:checked={form.enabled} disabled={saving} />
				<span><span class="strong">Enabled</span><span class="muted small">A front end that is off answers nothing and is never linked to.</span></span>
			</label>
			<label class="checkbox">
				<input type="checkbox" bind:checked={form.downloads} disabled={saving} />
				<span><span class="strong">Downloads</span><span class="muted small">Viewers may keep the files, not only play them.</span></span>
			</label>
		</div>

		<h3 class="sub">What it shows</h3>
		<Field label="Platforms" for="fe-profile" hint="The profile's toggles, applied on their own, decide which platforms' media the site shows.">
			<select id="fe-profile" class="select" bind:value={form.profile_id} disabled={saving}>
				{#each data.profiles as profile (profile.id)}
					<option value={profile.id}>{profile.name}</option>
				{/each}
			</select>
		</Field>
		<div class="grid-2">
			<Field label="Guilds" for="fe-guilds" optional hint="Media seen in these guilds. Empty, with no channels either, means everything the server has.">
				<TagInput id="fe-guilds" bind:values={form.scope.guilds} placeholder="Guild id, then Enter" validate={snowflake('guild')} disabled={saving} />
			</Field>
			<Field label="Channels" for="fe-channels" optional hint="Media seen in these channels, whatever their guild.">
				<TagInput id="fe-channels" bind:values={form.scope.channels} placeholder="Channel id, then Enter" validate={snowflake('channel')} disabled={saving} />
			</Field>
		</div>

		<h3 class="sub">Who gets in</h3>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.access.open} disabled={saving} />
			<span><span class="strong">Open to anyone</span><span class="muted small">No login at all. The ways below are not asked for.</span></span>
		</label>
		<div class="grid-2">
			<Field label="Shared secret" for="fe-secret-kind" optional hint={form.access.secret_kind ? SECRET_LABELS[form.access.secret_kind].hint : 'One secret everyone who may look shares.'}>
				<select id="fe-secret-kind" class="select" bind:value={form.access.secret_kind} disabled={saving || form.access.open}>
					<option value={null}>None</option>
					{#each SECRET_KINDS as kind (kind)}
						<option value={kind}>{SECRET_LABELS[kind].label}</option>
					{/each}
				</select>
			</Field>
			<Field label={editing?.has_secret ? 'Replace the secret' : 'Set the secret'} for="fe-secret" optional hint={editing?.has_secret ? 'A secret is stored; it is never shown. Leave empty to keep it.' : 'Stored hashed once saved; 4 to 200 characters.'}>
				<div class="row">
					<PasswordInput id="fe-secret" bind:value={secret} placeholder={form.access.secret_kind ? SECRET_LABELS[form.access.secret_kind].prompt : 'Pick a kind first'} autocomplete="off" disabled={saving || form.access.open || !form.access.secret_kind} />
					{#if editing?.has_secret}
						<Button size="sm" variant="danger-soft" onclick={clearSecret} disabled={saving}>Remove</Button>
					{/if}
				</div>
			</Field>
		</div>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.access.accounts} disabled={saving || form.access.open} />
			<span><span class="strong">Accounts of its own</span><span class="muted small">Username and password pairs managed from this page, apart from the server's accounts.</span></span>
		</label>
		<Field label="Login providers" for="fe-providers" optional hint={data.providers.length ? 'Viewers may sign in through these.' : 'No login provider is set up in the server settings.'}>
			<div id="fe-providers" class="stack-sm">
				{#each data.providers as provider (provider.id)}
					<label class="checkbox">
						<input type="checkbox" checked={form.access.providers.includes(provider.id)} onchange={(e) => toggleProvider(provider.id, (e.currentTarget as HTMLInputElement).checked)} disabled={saving || form.access.open} />
						<span><span>{provider.name}</span></span>
					</label>
				{/each}
			</div>
		</Field>
		{#if discordListed}
			<div class="grid-2">
				<label class="checkbox">
					<input type="checkbox" bind:checked={form.access.discord_members} disabled={saving || form.access.open || form.scope.guilds.length === 0} />
					<span><span class="strong">Discord members only</span><span class="muted small">{form.scope.guilds.length === 0 ? 'Needs a guild in the scope above.' : 'A Discord login must belong to every guild in the scope.'}</span></span>
				</label>
				<Field label="Discord users" for="fe-discord-users" optional hint="When any are listed, only these Discord users get in through Discord.">
					<TagInput id="fe-discord-users" bind:values={form.access.discord_users} placeholder="User id, then Enter" validate={snowflake('user')} disabled={saving || form.access.open} />
				</Field>
			</div>
		{/if}
		{#if noWayIn}
			<Alert tone="warn" message="Nobody can get in as this stands: it is not open and no secret, account or provider is set up." />
		{/if}

		<h3 class="sub">Instead of uploading</h3>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.links.enabled} disabled={saving} />
			<span><span class="strong">Post this site's link when an upload would be too large or too reduced</span><span class="muted small">The bots post the media's page here, which Discord plays inline, instead of a file squeezed under the upload limit. Needs web.public_url in the settings.</span></span>
		</label>
		<div class="grid-2">
			<Field label="Lowest height for an upload" for="fe-min-height" hint="An upload that would come out shorter than this, in pixels, becomes a link.">
				<input id="fe-min-height" class="input" type="number" min="1" bind:value={form.links.min_height} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Lowest bitrate for an upload" for="fe-min-bitrate" hint="In kb/s over the whole file.">
				<input id="fe-min-bitrate" class="input" type="number" min="1" bind:value={minBitrateKb} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Page bound" for="fe-max-bytes" hint={`In MiB: the most the media made for the page may take; ${formatBytes(Math.max(1, maxBytesMb) * 1024 * 1024)}.`}>
				<input id="fe-max-bytes" class="input" type="number" min="1" bind:value={maxBytesMb} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Signed links last" for="fe-signed-days" hint="Days a posted link plays the media without a login, however the site lets people in.">
				<input id="fe-signed-days" class="input" type="number" min="1" bind:value={form.links.signed_link_days} disabled={saving || !form.links.enabled} />
			</Field>
		</div>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={saving}>Cancel</Button>
		<Button variant="primary" loading={saving} onclick={() => document.querySelector<HTMLFormElement>('#frontend-form')?.requestSubmit()}>{editing ? 'Save' : 'Add front end'}</Button>
	{/snippet}
</Dialog>

<Dialog open={resetting !== null} title={resetting ? `Reset the password of ${resetting.username}` : 'Reset password'} size="sm" busy={savingReset} onclose={() => (resetting = null)}>
	<form id="reset-form" class="stack" onsubmit={saveReset} novalidate>
		{#if resetError}
			<Alert tone="danger" message={resetError} onclose={() => (resetError = null)} />
		{/if}
		<Field label="New password" for="fe-reset-password" hint="8 to 256 characters.">
			<PasswordInput id="fe-reset-password" bind:value={resetPassword} autocomplete="new-password" disabled={savingReset} />
		</Field>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (resetting = null)} disabled={savingReset}>Cancel</Button>
		<Button variant="primary" loading={savingReset} disabled={resetPassword === ''} onclick={() => document.querySelector<HTMLFormElement>('#reset-form')?.requestSubmit()}>Reset</Button>
	{/snippet}
</Dialog>

<style>
	.sub {
		margin: 6px 0 0;
		font-size: 14px;
		font-weight: 600;
	}

	.add {
		flex-wrap: wrap;
	}

	.add :global(.input) {
		flex: 1 1 140px;
	}

	.block {
		display: block;
	}

	.plain {
		list-style: none;
		margin: 0;
		padding: 0;
	}
</style>
