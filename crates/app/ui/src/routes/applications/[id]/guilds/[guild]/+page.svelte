<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, profiles as profilesApi, rules } from '$lib/api';
	import type { Assignment, EffectiveView, GuildChannel, Rule, RuleInput, Scope } from '$lib/api';
	import ChannelSelect from '$lib/components/ChannelSelect.svelte';
	import MemberPicker from '$lib/components/MemberPicker.svelte';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RuleForm from '$lib/components/RuleForm.svelte';
	import Time from '$lib/components/Time.svelte';
	import {
		CHANNEL_KIND_LABELS,
		channelGlyph,
		channelName,
		groupChannels,
		memberLookup,
		memberSearch,
		takenChannels,
		unwatchableReason,
		watchable
	} from '$lib/discord';
	import { describeScope, profileName } from '$lib/profiles';
	import { pluralize, shortId } from '$lib/format';
	import { cleanInput, describeFilters, describeLimits, emptyRule, toInput } from '$lib/rules';
	import { bots } from '$lib/state/bots.svelte';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const status = $derived(bots.status(data.applicationId) ?? data.app?.bot ?? null);
	const appName = $derived(
		data.app?.name ??
			(status?.state === 'connected' ? status.user : `Application ${shortId(data.applicationId)}`)
	);
	const guildName = $derived(data.botGuild?.name ?? data.myGuild?.name ?? `Guild ${data.guildId}`);
	const guildIcon = $derived(data.botGuild?.icon ?? data.myGuild?.icon ?? null);
	const present = $derived(data.botGuild?.present ?? false);
	const channels = $derived(data.directory.channels);
	const removed = $derived(data.botGuild !== null && !present);
	const notJoined = $derived(data.botGuild === null && data.app !== null);
	const notConnected = $derived(status !== null && status.state !== 'connected');
	const crumbs = $derived(
		session.can('manage_applications') && data.app
			? [
					{ label: 'Applications', href: '/applications' },
					{ label: data.app.name, href: `/applications/${data.applicationId}` },
					{ label: guildName }
				]
			: session.can('manage_watch_rules')
				? [{ label: 'Watch rules', href: '/rules' }, { label: guildName }]
				: [{ label: 'My guilds', href: '/guilds' }, { label: guildName }]
	);
	const refresh = () => invalidate(`app:guild:${data.applicationId}:${data.guildId}`);

	// Channels

	let channelFilter = $state('');
	const rulesByChannel = $derived(new Map(data.rules.map((rule) => [rule.channel_id, rule])));
	const messageChannels = $derived((channels ?? []).filter((c) => watchable(c.kind)));
	const watchedCount = $derived(messageChannels.filter((c) => rulesByChannel.has(c.id)).length);
	const groups = $derived.by(() => {
		if (!channels) return [];
		const needle = channelFilter.trim().toLowerCase();
		return groupChannels(channels)
			.map((group) => ({
				...group,
				channels: group.channels.filter(
					(c) => !needle || c.name.toLowerCase().includes(needle) || c.id.includes(needle)
				)
			}))
			.filter((group) => group.channels.length > 0);
	});

	// Add and edit

	let dialog = $state(false);
	let editing = $state<Rule | null>(null);
	let form = $state<RuleInput>(emptyRule());
	let formRef = $state<RuleForm | undefined>();
	let saving = $state(false);
	let error = $state<string | null>(null);
	const taken = $derived(takenChannels(data.rules, editing?.id));

	function openAdd(channel?: GuildChannel) {
		editing = null;
		form = { ...emptyRule(), channel_id: channel?.id ?? '' };
		error = null;
		dialog = true;
	}

	function openEdit(rule: Rule) {
		editing = rule;
		form = toInput(rule);
		error = null;
		dialog = true;
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!formRef?.valid()) {
			error = 'Fix the highlighted fields first.';
			return;
		}
		saving = true;
		error = null;
		const input = cleanInput(form);
		const name = channelName(channels, input.channel_id);
		try {
			if (editing) {
				await rules.update(editing.id, input);
				toast.ok(`Rule for ${name} saved.`);
			} else {
				await rules.createForGuild(data.applicationId, data.guildId, input);
				toast.ok(`Now watching ${name}.`);
			}
			dialog = false;
			await refresh();
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	// Enable, disable, delete

	let toggling = $state<string | null>(null);
	let deleting = $state<string | null>(null);

	async function toggle(rule: Rule) {
		const name = channelName(channels, rule.channel_id);
		toggling = rule.id;
		try {
			await rules.update(rule.id, { ...toInput(rule), enabled: !rule.enabled });
			toast.ok(rule.enabled ? `${name} is no longer watched.` : `${name} is watched again.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not change the rule: ${messageOf(cause)}`);
		} finally {
			toggling = null;
		}
	}

	// Profiles

	/** The session may put profiles in force here when the server listed the guild's assignments. */
	const canAssign = $derived(data.assignments !== null);
	const globalAssignment = $derived(
		data.assignments?.find((a) => a.scope.kind === 'global') ?? null
	);
	const guildAssignment = $derived(
		data.assignments?.find((a) => a.scope.kind === 'guild') ?? null
	);
	const channelAssignments = $derived(
		(data.assignments ?? []).filter((a): a is Assignment & { scope: { kind: 'channel' } } => a.scope.kind === 'channel')
	);
	const userAssignments = $derived(
		(data.assignments ?? []).filter((a): a is Assignment & { scope: { kind: 'user' } } => a.scope.kind === 'user')
	);
	const nameOf = (id: string) => profileName(data.profiles, id);

	let guildChoice = $state('');
	$effect(() => {
		guildChoice = guildAssignment?.profile_id ?? '';
	});
	let channelChoice = $state('');
	let channelProfile = $state('');
	let userChoice = $state<string[]>([]);
	let userProfile = $state('');
	let assigning = $state<string | null>(null);

	async function putInForce(scope: Scope, profile: string, what: string) {
		assigning = scopeLabel(scope);
		try {
			await profilesApi.assign(scope, profile);
			toast.ok(`${nameOf(profile)} is now in force for ${what}.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not put the profile in force: ${messageOf(cause)}`);
		} finally {
			assigning = null;
		}
	}

	async function takeOff(scope: Scope, what: string) {
		assigning = scopeLabel(scope);
		try {
			await profilesApi.unassign(scope);
			toast.ok(`${what} follows the wider scope again.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not take the profile off: ${messageOf(cause)}`);
		} finally {
			assigning = null;
		}
	}

	function scopeLabel(scope: Scope): string {
		switch (scope.kind) {
			case 'global':
				return 'global';
			case 'guild':
				return `guild:${scope.guild_id}`;
			case 'channel':
				return `channel:${scope.guild_id}:${scope.channel_id}`;
			case 'user':
				return `user:${scope.guild_id}:${scope.user_id}`;
		}
	}

	const guildScope = $derived<Scope>({ kind: 'guild', guild_id: data.guildId });

	async function saveGuildProfile() {
		if (!guildChoice) return;
		await putInForce(guildScope, guildChoice, 'this guild');
	}

	async function addChannelAssignment() {
		if (!channelChoice || !channelProfile) return;
		const scope: Scope = { kind: 'channel', guild_id: data.guildId, channel_id: channelChoice };
		await putInForce(scope, channelProfile, describeScope(scope, channels));
		channelChoice = '';
		channelProfile = '';
	}

	async function addUserAssignments() {
		if (userChoice.length === 0 || !userProfile) return;
		for (const user of userChoice) {
			const scope: Scope = { kind: 'user', guild_id: data.guildId, user_id: user };
			await putInForce(scope, userProfile, describeScope(scope));
		}
		userChoice = [];
		userProfile = '';
	}

	// What is in force for a channel, and a member in it

	let checkChannel = $state('');
	let checkUser = $state<string[]>([]);
	let checking = $state(false);
	let checked = $state<EffectiveView | null>(null);
	let checkError = $state<string | null>(null);

	async function runCheck() {
		checking = true;
		checkError = null;
		try {
			checked = await profilesApi.effective(data.guildId, checkChannel || undefined, checkUser[0]);
		} catch (cause) {
			checked = null;
			checkError = messageOf(cause);
		} finally {
			checking = false;
		}
	}

	const checkedOff = $derived(checked ? Object.entries(checked.platforms).filter(([, on]) => !on).map(([id]) => id) : []);

	async function remove(rule: Rule) {
		const name = channelName(channels, rule.channel_id);
		const ok = await confirm.ask({
			title: `Stop watching ${name}?`,
			message: 'The rule is removed. Links posted there are no longer picked up.',
			confirmLabel: 'Remove rule',
			danger: true
		});
		if (!ok) return;
		deleting = rule.id;
		try {
			await rules.remove(rule.id);
			toast.ok('Rule removed.');
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the rule: ${messageOf(cause)}`);
		} finally {
			deleting = null;
		}
	}
</script>

<svelte:head>
	<title>{guildName} · Watch rules · DiscoClip</title>
</svelte:head>

{#snippet channelRef(id: string, href?: string)}
	{#if channels?.some((c) => c.id === id)}
		<span class="stack-sm" style="gap:0">
			{#if href}<a {href} class="row-link">{channelName(channels, id)}</a>{:else}<span>{channelName(channels, id)}</span>{/if}
			<code class="small">{id}</code>
		</span>
	{:else if href}
		<a {href} class="row-link"><code>{id}</code></a>
	{:else}
		<code>{id}</code>
	{/if}
{/snippet}

<PageHeader title={guildName} {crumbs}>
	{#snippet meta()}
		<span class="row">
			<GuildIcon id={data.guildId} icon={guildIcon} name={guildName} size={24} />
			<code class="small">{data.guildId}</code>
		</span>
		<span class="faint small">·</span>
		<span class="row faint small">
			{appName}
			{#if status}<BotBadge state={status.state} size="sm" />{/if}
		</span>
		{#if data.botGuild}
			{#if present}
				<Badge tone="ok" size="sm" dot>Bot present</Badge>
			{:else}
				<Badge tone="warn" size="sm">Bot removed <Time value={data.botGuild.left_at} /></Badge>
			{/if}
		{/if}
	{/snippet}
	{#snippet actions()}
		{#if data.install}
			<Button href={data.install.url} newTab icon="external">Add bot to this guild</Button>
		{/if}
		<Button variant="primary" icon="plus" onclick={() => openAdd()}>Add rule</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	{#if removed || notJoined || notConnected}
		<div class="stack">
			{#if removed}
				<Alert tone="warn" title="The bot was removed from this guild" message="Rules stay, but nothing is watched until the bot is added again." />
			{:else if notJoined}
				<Alert tone="info" title="The bot has not joined this guild" message="Use the install link to add it. Rules can be prepared now." />
			{/if}
			{#if notConnected}
				<Alert tone="info" message="Listing channels and adding or changing a rule go through the bot, so the bot must be connected." />
			{/if}
		</div>
	{/if}

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Channels</h2>
				<p class="hint">The channels the bot sees in this guild. Watch one to add a rule for it.</p>
			</div>
			{#if channels}
				<div class="row">
					<span class="faint small">{watchedCount} of {pluralize(messageChannels.length, 'message channel')} watched</span>
					<input class="input filter" type="search" placeholder="Filter channels" bind:value={channelFilter} aria-label="Filter channels" />
				</div>
			{/if}
		</div>
		{#if channels}
			{#if groups.length === 0}
				<div class="card-body">
					<Empty
						compact
						icon={channelFilter ? 'search' : 'hash'}
						title={channelFilter ? 'No channel matches' : 'The bot sees no channels'}
						description={channelFilter ? 'No channel name or id contains that text.' : 'Give the bot access to channels on Discord, then reload.'}
					/>
				</div>
			{:else}
				<div class="channels">
					{#each groups as group, i (group.category?.id ?? `loose-${i}`)}
						<div class="group">
							{#if group.category}
								<div class="category"><Icon name="folder" size={13} /><span>{group.category.name}</span></div>
							{/if}
							{#each group.channels as channel (channel.id)}
								{@const rule = rulesByChannel.get(channel.id)}
								<div class={['channel', rule && 'watched', !watchable(channel.kind) && 'inert']}>
									<span class="glyph"><Icon name={channelGlyph(channel.kind)} size={15} /></span>
									<span class="name">
										<span class="truncate">{channel.name}</span>
										{#if channel.kind !== 'text'}<span class="faint small">{CHANNEL_KIND_LABELS[channel.kind]}</span>{/if}
									</span>
									<span class="state">
										{#if rule}
											{#if rule.enabled}<Badge tone="ok" size="sm" dot>Watched</Badge>{:else}<Badge size="sm">Rule disabled</Badge>{/if}
											{#if rule.post_to && rule.post_to !== channel.id}
												<span class="faint small">posts to {channelName(channels, rule.post_to)}</span>
											{/if}
										{:else if !watchable(channel.kind)}
											<span class="faint small">{unwatchableReason(channel.kind)}</span>
										{/if}
									</span>
									<span class="actions">
										{#if rule}
											<Button size="sm" variant="ghost" icon="pencil" onclick={() => openEdit(rule)}>Edit rule</Button>
										{:else if watchable(channel.kind)}
											<Button size="sm" icon="plus" onclick={() => openAdd(channel)}>Watch</Button>
										{/if}
									</span>
								</div>
							{/each}
						</div>
					{/each}
				</div>
			{/if}
		{:else}
			<div class="card-body stack-sm">
				<Alert tone="warn" title="The bot could not list the channels" message={data.directory.channelsError ?? ''} />
				<p class="hint">Listing the channels needs the bot connected and in the guild, and so does saving a rule. Rules below are shown by channel id.</p>
			</div>
		{/if}
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Rules</h2>
				<p class="hint">Where results go, whose links count and how big a video may be, per watched channel.</p>
			</div>
			<span class="faint small">{pluralize(data.rules.length, 'rule')}</span>
		</div>
		{#if data.rules.length === 0}
			<div class="card-body">
				<Empty compact icon="rules" title="No rules for this guild" description="A rule names a channel to watch, where results go, whose links count and how big a video may be.">
					<Button variant="primary" icon="plus" onclick={() => openAdd()}>Add the first rule</Button>
				</Empty>
			</div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Watched channel</th>
							<th>Posts to</th>
							<th>Who and where from</th>
							<th>Limits</th>
							<th>Status</th>
							<th>Updated</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each data.rules as rule (rule.id)}
							<tr class={[!rule.enabled && 'off']}>
								<td>{@render channelRef(rule.channel_id, `/rules/${rule.id}`)}</td>
								<td>{#if rule.post_to}{@render channelRef(rule.post_to)}{:else}<span class="faint">same channel</span>{/if}</td>
								<td>{describeFilters(rule)}</td>
								<td>{describeLimits(rule)}</td>
								<td>
									{#if rule.enabled}<Badge tone="ok" size="sm" dot>Enabled</Badge>{:else}<Badge size="sm">Disabled</Badge>{/if}
								</td>
								<td><Time value={rule.updated_at} /></td>
								<td class="actions">
									<Button size="sm" variant="ghost" loading={toggling === rule.id} onclick={() => toggle(rule)}>{rule.enabled ? 'Disable' : 'Enable'}</Button>
									<Button size="sm" variant="ghost" icon="pencil" onclick={() => openEdit(rule)}>Edit</Button>
									<Button size="sm" variant="ghost" icon="trash" loading={deleting === rule.id} onclick={() => remove(rule)} title="Remove rule" square />
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Profiles</h2>
				<p class="hint">Which platforms are on here. A profile for the guild sits under the server's; a channel's or a member's sits under the guild's, and the narrowest wins. <a href="/profiles">See the profiles.</a></p>
			</div>
			{#if canAssign}
				<span class="faint small">{pluralize(channelAssignments.length, 'channel')} · {pluralize(userAssignments.length, 'member')} with one of their own</span>
			{/if}
		</div>
		<div class="card-body stack">
			<div class="scope">
				<div class="scope-text">
					<span class="strong">This guild</span>
					{#if guildAssignment}
						<span class="muted">{nameOf(guildAssignment.profile_id)} is in force, on top of the server's {globalAssignment ? nameOf(globalAssignment.profile_id) : 'profile'}.</span>
					{:else}
						<span class="muted">Follows the server's {globalAssignment ? nameOf(globalAssignment.profile_id) : 'profile'}.</span>
					{/if}
				</div>
				{#if canAssign}
					<div class="row">
						<select class="select" bind:value={guildChoice} aria-label="Profile for this guild" disabled={assigning !== null}>
							<option value="" disabled>Choose a profile</option>
							{#each data.profiles as profile (profile.id)}
								<option value={profile.id}>{profile.name}</option>
							{/each}
						</select>
						<Button size="sm" variant="primary" loading={assigning === scopeLabel(guildScope)} disabled={!guildChoice || guildChoice === (guildAssignment?.profile_id ?? '')} onclick={saveGuildProfile}>Put in force</Button>
						{#if guildAssignment}
							<Button size="sm" variant="ghost" icon="x" disabled={assigning !== null} onclick={() => takeOff(guildScope, 'This guild')}>Take off</Button>
						{/if}
					</div>
				{/if}
			</div>

			{#if canAssign}
				<div class="grid-2">
					<div class="stack-sm">
						<span class="strong">Channels</span>
						{#if channelAssignments.length === 0}
							<span class="faint small">No channel has a profile of its own.</span>
						{:else}
							<ul class="plain assignments">
								{#each channelAssignments as assignment (scopeLabel(assignment.scope))}
									<li class="row-between">
										<span class="row"><span>{describeScope(assignment.scope, channels)}</span><span class="faint small">→ {nameOf(assignment.profile_id)}</span></span>
										<Button size="sm" variant="ghost" icon="x" title="Take the profile off this channel" square disabled={assigning !== null} onclick={() => takeOff(assignment.scope, describeScope(assignment.scope, channels))} />
									</li>
								{/each}
							</ul>
						{/if}
						{#if channels}
							<div class="row">
								<ChannelSelect id="pf-channel" bind:value={channelChoice} {channels} disabled={assigning !== null} />
								<select class="select" bind:value={channelProfile} aria-label="Profile for the channel" disabled={assigning !== null}>
									<option value="" disabled>Choose a profile</option>
									{#each data.profiles as profile (profile.id)}
										<option value={profile.id}>{profile.name}</option>
									{/each}
								</select>
								<Button size="sm" icon="plus" disabled={!channelChoice || !channelProfile || assigning !== null} onclick={addChannelAssignment}>Put in force</Button>
							</div>
						{:else}
							<span class="faint small">Channels are picked by name once the bot can list them.</span>
						{/if}
					</div>
					<div class="stack-sm">
						<span class="strong">Members</span>
						{#if userAssignments.length === 0}
							<span class="faint small">No member has a profile of their own.</span>
						{:else}
							<ul class="plain assignments">
								{#each userAssignments as assignment (scopeLabel(assignment.scope))}
									<li class="row-between">
										<span class="row"><code>{assignment.scope.user_id}</code><span class="faint small">→ {nameOf(assignment.profile_id)}</span></span>
										<Button size="sm" variant="ghost" icon="x" title="Take the profile off this member" square disabled={assigning !== null} onclick={() => takeOff(assignment.scope, `Member ${assignment.scope.user_id}`)} />
									</li>
								{/each}
							</ul>
						{/if}
						<MemberPicker id="pf-user" bind:values={userChoice} search={memberSearch(data.directory)} lookup={memberLookup(data.directory)} disabled={assigning !== null} />
						<div class="row">
							<select class="select" bind:value={userProfile} aria-label="Profile for the members" disabled={assigning !== null}>
								<option value="" disabled>Choose a profile</option>
								{#each data.profiles as profile (profile.id)}
									<option value={profile.id}>{profile.name}</option>
								{/each}
							</select>
							<Button size="sm" icon="plus" disabled={userChoice.length === 0 || !userProfile || assigning !== null} onclick={addUserAssignments}>Put in force</Button>
						</div>
					</div>
				</div>

				<div class="check stack-sm">
					<span class="strong">What is in force</span>
					<span class="hint">Pick a channel and, if you like, a member, to see which platforms are off there and which profiles made it so.</span>
					<div class="row">
						{#if channels}
							<ChannelSelect id="pf-check-channel" bind:value={checkChannel} {channels} emptyLabel="Anywhere in the guild" disabled={checking} />
						{:else}
							<input class="input" bind:value={checkChannel} placeholder="Channel id" aria-label="Channel id" disabled={checking} />
						{/if}
						<Button size="sm" icon="search" loading={checking} onclick={runCheck}>Check</Button>
					</div>
					<MemberPicker id="pf-check-user" bind:values={checkUser} search={memberSearch(data.directory)} lookup={memberLookup(data.directory)} disabled={checking} />
					{#if checkError}
						<Alert tone="danger" message={checkError} onclose={() => (checkError = null)} />
					{:else if checked}
						<div class="stack-sm">
							<span class="small">
								{#if checkedOff.length === 0}Every platform is on.{:else}{pluralize(checkedOff.length, 'platform')} off:{/if}
							</span>
							{#if checkedOff.length}
								<div class="chips">{#each checkedOff as id (id)}<span class="chip">{id}</span>{/each}</div>
							{/if}
							<span class="faint small">
								Applied: {checked.applied.map((a) => `${nameOf(a.profile_id)} for ${describeScope(a.scope, channels)}`).join(', then ')}.
							</span>
						</div>
					{/if}
				</div>
			{/if}
		</div>
	</section>
</div>

<Dialog
	bind:open={dialog}
	title={editing ? `Edit rule for ${channelName(channels, editing.channel_id)}` : 'Add a watch rule'}
	size="lg"
	busy={saving}
>
	<form id="rule-form" class="stack" onsubmit={save} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<RuleForm id="rule" bind:value={form} bind:this={formRef} disabled={saving} directory={data.directory} {taken} />
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={saving}>Cancel</Button>
		<Button variant="primary" loading={saving} onclick={() => document.querySelector<HTMLFormElement>('#rule-form')?.requestSubmit()}>{editing ? 'Save rule' : 'Add rule'}</Button>
	{/snippet}
</Dialog>

<style>
	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.filter {
		width: 220px;
	}

	.channels {
		display: flex;
		flex-direction: column;
		padding: 6px 0;
	}

	.group {
		display: flex;
		flex-direction: column;
	}

	.category {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 12px 20px 4px;
		font-size: 11.5px;
		font-weight: 600;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--text-3);
	}

	.channel {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto auto;
		align-items: center;
		gap: 10px;
		padding: 6px 20px;
	}

	.channel:hover {
		background: color-mix(in srgb, var(--surface-2) 60%, transparent);
	}

	.channel.inert .name {
		color: var(--text-3);
	}

	.glyph {
		display: flex;
		color: var(--text-3);
	}

	.channel.watched .glyph {
		color: var(--ok-text);
	}

	.name {
		display: flex;
		align-items: baseline;
		gap: 8px;
		min-width: 0;
		font-weight: 500;
	}

	.state {
		display: flex;
		align-items: center;
		gap: 8px;
		justify-content: flex-end;
		flex-wrap: wrap;
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		min-width: 92px;
	}

	tr.off td {
		color: var(--text-3);
	}

	.scope {
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

	.scope-text {
		display: flex;
		flex-direction: column;
		gap: 4px;
		min-width: 0;
	}

	.assignments li {
		padding: 4px 0;
		border-bottom: 1px solid var(--border);
	}

	.assignments li:last-child {
		border-bottom: none;
	}

	.check {
		padding: 12px 14px;
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		background: var(--surface-2);
	}

	@media (max-width: 640px) {
		.channel {
			grid-template-columns: auto minmax(0, 1fr) auto;
		}

		.state {
			grid-column: 2 / 4;
			justify-content: flex-start;
		}

		.filter {
			width: 100%;
		}
	}
</style>
