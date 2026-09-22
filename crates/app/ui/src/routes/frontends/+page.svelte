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
	import FormFeedback from '$lib/components/FormFeedback.svelte';
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
	let section = $state('basics');
	const sections = [{ id: 'basics', label: 'Site details' }, { id: 'media', label: 'Media' }, { id: 'access', label: 'Access' }, { id: 'discord', label: 'Discord links' }];
	let editing = $state<Frontend | null>(null);
	let form = $state<FrontendInput>(emptyFrontend(''));
	let saving = $state(false);
	let error = $state<string | null>(null);
	let minBitrateKb = $state(1500);
	let maxBytesMb = $state(2048);
	let secret = $state('');

	function openAdd() {
		section = 'basics';
		secret = '';
		editing = null;
		form = emptyFrontend(defaultProfile);
		minBitrateKb = Math.round(form.links.min_bitrate / 1000);
		maxBytesMb = Math.round(form.links.max_bytes / (1024 * 1024));
		error = null;
		dialog = true;
	}

	function openEdit(frontend: Frontend) {
		section = 'basics';
		secret = '';
		editing = frontend;
		form = toInput(frontend);
		minBitrateKb = Math.round(form.links.min_bitrate / 1000);
		maxBytesMb = Math.round(form.links.max_bytes / (1024 * 1024));
		error = null;
		dialog = true;
	}

	const nameProblem = $derived(form.name.trim() === '' ? 'Enter a site name.' : null);
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
			section = 'basics';
			error = nameProblem ?? slugIssue;
			return;
		}
		if (secret && (secret.trim().length < 4 || secret.trim().length > 200)) {
			section = 'access';
			error = 'Use 4 to 200 characters for the shared secret.';
			return;
		}
		if (form.links.enabled && (![form.links.min_height, form.links.signed_link_days].every((n) => Number.isInteger(n) && n > 0) || ![minBitrateKb, maxBytesMb].every((n) => Number.isFinite(n) && n > 0))) {
			section = 'discord';
			error = 'Enter positive values for the link limits. Height and expiry must be whole numbers.';
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
		const creating = editing === null;
		try {
			let saved: Frontend;
			if (editing) {
				saved = await api.update(editing.id, input);
			} else {
				saved = await api.create(input);
			}
			editing = saved;
			if (secret.trim() !== '') {
				await api.setSecret(saved.id, secret.trim());
				secret = '';
			}
			toast.ok(`Media site ${saved.name} ${creating ? 'created' : 'saved'}.`);
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
			message: 'The secret will stop working. Existing sessions stay active.',
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
			title: `Remove the media site ${frontend.name}?`,
			message: `/f/${frontend.slug} will close. Its accounts and sessions will be deleted. Bots will upload files instead.`,
			confirmLabel: 'Remove media site',
			danger: true
		});
		if (!ok) return;
		deleting = frontend.id;
		try {
			await api.remove(frontend.id);
			toast.ok(`Media site ${frontend.name} removed.`);
			if (detail?.id === frontend.id) detail = null;
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the media site: ${messageOf(cause)}`);
		} finally {
			deleting = null;
		}
	}

	// Accounts and sessions of one media site

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
			message: 'Active sessions will end.',
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
			message: 'Everyone logged into the media site has to log in again.',
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
	<title>Media sites · DiscoClip</title>
</svelte:head>

<PageHeader title={dialog ? (editing ? editing.name : 'New media site') : 'Media sites'} description={dialog ? undefined : 'Share clips through a public or private website.'}>
	{#snippet actions()}
		{#if dialog}
			<Button icon="arrow-left" onclick={() => (dialog = false)} disabled={saving}>All sites</Button>
		{:else}
			<Button variant="primary" icon="plus" onclick={openAdd}>Create site</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack-lg" hidden={dialog}>
	<section class="card">
		<div class="card-header">
			<div>
				<h2>Media sites</h2>
				<p class="hint">Each lives at /f/&lt;slug&gt; on this server.</p>
			</div>
			<span class="faint small">{pluralize(data.frontends.length, 'media site')}</span>
		</div>
		{#if data.frontends.length === 0}
			<div class="card-body"><Empty compact icon="monitor" title="No media sites" description="The server hosts no public site yet." /></div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Media site</th>
							<th>Shows</th>
							<th>Access</th>
							<th>Discord output</th>
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
									<Button size="sm" variant="ghost" icon="trash" loading={deleting === frontend.id} title="Remove media site" onclick={() => remove(frontend)} square />
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
					<p class="hint">The media site's Site accounts, and who is logged in right now.</p>
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
							<p class="muted small">Enable site accounts in Access to allow these users to log in.</p>
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
						<form class="stack add" onsubmit={addUser}>
							<Field label="Username" for="fe-new-username"><input id="fe-new-username" class="input" placeholder="Username" bind:value={newUsername} maxlength="40" autocomplete="off" disabled={addingUser} aria-label="New account username" /></Field>
							<Field label="Password" for="fe-new-password"><PasswordInput id="fe-new-password" bind:value={newPassword} placeholder="Password" autocomplete="new-password" disabled={addingUser} /></Field>
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

{#if dialog}
	<nav class="editor-tabs" aria-label="Site settings">
		{#each sections as item (item.id)}
			<button type="button" class:active={section === item.id} aria-current={section === item.id ? 'step' : undefined} onclick={() => (section = item.id)}>{item.label}</button>
		{/each}
	</nav>
	<form id="frontend-form" class="site-editor" onsubmit={save} novalidate>
		{#if error}
			<FormFeedback message={error} />
		{/if}
		<fieldset class="form-section" hidden={section !== 'basics'}>
			<legend>Site details</legend>
		<div class="form-stack">
			<Field label="Name" for="fe-name" error={error && nameProblem ? nameProblem : null}>
				<input id="fe-name" class="input" bind:value={form.name} maxlength="80" disabled={saving} autocomplete="off" />
			</Field>
			<Field label="Slug" for="fe-slug" hint={`The site lives at /f/${form.slug.trim() || '<slug>'}.`} error={error || form.slug.trim() ? slugIssue : null}>
				<input id="fe-slug" class="input mono" bind:value={form.slug} maxlength="40" disabled={saving} autocomplete="off" spellcheck="false" />
			</Field>
		</div>
		<Field label="Description" for="fe-description" optional hint="Shown under the name on the site.">
			<input id="fe-description" class="input" bind:value={form.description} maxlength="500" disabled={saving} autocomplete="off" />
		</Field>
		<div class="grid-2">
			<label class="checkbox">
				<input type="checkbox" bind:checked={form.enabled} disabled={saving} />
				<span><span class="strong">Enabled</span><span class="muted small">Make this site available to viewers.</span></span>
			</label>
			<label class="checkbox">
				<input type="checkbox" bind:checked={form.downloads} disabled={saving} />
				<span><span class="strong">Downloads</span><span class="muted small">Allow viewers to download files.</span></span>
			</label>
		</div>

		</fieldset>
		<fieldset class="form-section" hidden={section !== 'media'}>
		<legend>Media</legend>
		<Field label="Platforms" for="fe-profile" hint="The selected profile controls which platforms appear on this site.">
			<select id="fe-profile" class="select" bind:value={form.profile_id} disabled={saving}>
				{#each data.profiles as profile (profile.id)}
					<option value={profile.id}>{profile.name}</option>
				{/each}
			</select>
		</Field>
		<div class="grid-2">
			<Field label="Servers" for="fe-guilds" optional hint="Leave servers and channels empty to include all media.">
				<TagInput id="fe-guilds" bind:values={form.scope.guilds} placeholder="Server ID, then Enter" validate={snowflake('guild')} disabled={saving} />
			</Field>
			<Field label="Channels" for="fe-channels" optional hint="Media seen in these channels, whatever their guild.">
				<TagInput id="fe-channels" bind:values={form.scope.channels} placeholder="Channel id, then Enter" validate={snowflake('channel')} disabled={saving} />
			</Field>
		</div>

		</fieldset>
		<fieldset class="form-section" hidden={section !== 'access'}>
		<legend>Access</legend>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.access.open} disabled={saving} />
			<span><span class="strong">Open to anyone</span><span class="muted small">Viewers do not need to log in.</span></span>
		</label>
		<div class="stack" hidden={form.access.open}>
		<div class="grid-2">
			<Field label="Shared secret" for="fe-secret-kind" optional hint={form.access.secret_kind ? SECRET_LABELS[form.access.secret_kind].hint : 'A code or password shared by all viewers.'}>
				<select id="fe-secret-kind" class="select" bind:value={form.access.secret_kind} disabled={saving || form.access.open}>
					<option value={null}>None</option>
					{#each SECRET_KINDS as kind (kind)}
						<option value={kind}>{SECRET_LABELS[kind].label}</option>
					{/each}
				</select>
			</Field>
			<Field label={editing?.has_secret ? 'Replace the secret' : 'Set the secret'} for="fe-secret" optional hint={editing?.has_secret ? 'Leave blank to keep the current secret.' : 'Use 4 to 200 characters.'}>
				<div class="row">
					<PasswordInput id="fe-secret" bind:value={secret} placeholder={form.access.secret_kind ? SECRET_LABELS[form.access.secret_kind].prompt : 'Choose a secret type'} autocomplete="off" disabled={saving || form.access.open || !form.access.secret_kind} />
					{#if editing?.has_secret}
						<Button size="sm" variant="danger-soft" onclick={clearSecret} disabled={saving}>Remove</Button>
					{/if}
				</div>
			</Field>
		</div>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.access.accounts} disabled={saving || form.access.open} />
			<span><span class="strong">Site accounts</span><span class="muted small">Manage site accounts separately from DiscoClip accounts.</span></span>
		</label>
		<Field label="Login providers" for="fe-providers" optional hint={data.providers.length ? 'Allow login through these providers.' : 'Configure login providers in Settings.'}>
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
					<span><span class="strong">Discord members only</span><span class="muted small">{form.scope.guilds.length === 0 ? 'Choose at least one server under Media.' : 'Require membership in all selected servers.'}</span></span>
				</label>
				<Field label="Discord users" for="fe-discord-users" optional hint="Leave empty to allow any Discord user who meets the membership rules.">
					<TagInput id="fe-discord-users" bind:values={form.access.discord_users} placeholder="User id, then Enter" validate={snowflake('user')} disabled={saving || form.access.open} />
				</Field>
			</div>
		{/if}
		</div>
		{#if noWayIn}
			<Alert tone="warn" message="Choose an access method so viewers can log in." />
		{/if}

		</fieldset>
		<fieldset class="form-section" hidden={section !== 'discord'}>
		<legend>Discord links</legend>
		<label class="checkbox">
			<input type="checkbox" bind:checked={form.links.enabled} disabled={saving} />
			<span><span class="strong">Link to this site when an upload exceeds its limits</span><span class="muted small">Requires a public URL in Settings. Posted links include a media preview.</span></span>
		</label>
		<div class="grid-2" hidden={!form.links.enabled}>
			<Field label="Minimum upload height (px)" for="fe-min-height" hint="Post a link if the upload would fall below this height.">
				<input id="fe-min-height" class="input" type="number" min="1" bind:value={form.links.min_height} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Minimum upload bitrate (kb/s)" for="fe-min-bitrate" hint="Post a link if the upload would fall below this bitrate.">
				<input id="fe-min-bitrate" class="input" type="number" min="1" bind:value={minBitrateKb} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Maximum media size (MiB)" for="fe-max-bytes" hint={`In MiB: the most the media made for the page may take; ${formatBytes(Math.max(1, maxBytesMb) * 1024 * 1024)}.`}>
				<input id="fe-max-bytes" class="input" type="number" min="1" bind:value={maxBytesMb} disabled={saving || !form.links.enabled} />
			</Field>
			<Field label="Link expiry (days)" for="fe-signed-days" hint="Days a shared media link works without login.">
				<input id="fe-signed-days" class="input" type="number" min="1" bind:value={form.links.signed_link_days} disabled={saving || !form.links.enabled} />
			</Field>
		</div>
		</fieldset>
	</form>
	<div class="form-actions">
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={saving}>Cancel</Button>
		<Button type="submit" form="frontend-form" variant="primary" loading={saving}>{editing ? 'Save site' : 'Create site'}</Button>
	</div>
{/if}

<Dialog open={resetting !== null} title={resetting ? `Reset the password of ${resetting.username}` : 'Reset password'} size="sm" busy={savingReset} onclose={() => (resetting = null)}>
	<form id="reset-form" class="stack" onsubmit={saveReset} novalidate>
		{#if resetError}
			<FormFeedback message={resetError} />
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
	.site-editor { max-width: 760px; margin-bottom: 24px; padding: 0 24px 24px; border: 1px solid var(--border); border-radius: var(--radius); background: var(--surface); }
	.editor-tabs { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 24px; }
	.editor-tabs button { min-height: 44px; padding: 10px 16px; border: 1px solid var(--border-strong); border-radius: var(--radius-sm); background: var(--surface); cursor: pointer; }
	.editor-tabs button.active { background: var(--accent-soft); border-color: var(--accent); color: var(--accent-text); font-weight: 600; }
	.form-actions { max-width: 760px; }

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
