<script lang="ts">
	import Field from '$lib/components/Field.svelte';
	import FormFeedback from '$lib/components/FormFeedback.svelte';
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, profiles as profilesApi, rules } from '$lib/api';
	import type { RuleInput, Scope } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import SectionNav from '$lib/components/SectionNav.svelte';
	import RuleForm from '$lib/components/RuleForm.svelte';
	import Time from '$lib/components/Time.svelte';
	import { channelById, channelName, takenChannels } from '$lib/discord';
	import { shortId } from '$lib/format';
	import { untrack } from 'svelte';
	import { describeLimits, inForceFor, profileName } from '$lib/profiles';
	import { cleanInput, emptyRule, sameInput, toInput } from '$lib/rules';
	import { bots } from '$lib/state/bots.svelte';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const rule = $derived(data.rule);
	const channels = $derived(data.directory.channels);
	const status = $derived(bots.status(rule.application_id) ?? data.app?.bot ?? null);
	const appName = $derived(
		data.app?.name ?? (status?.state === 'connected' ? status.user : `Application ${shortId(rule.application_id)}`)
	);
	const guildName = $derived(data.botGuild?.name ?? `Server ${rule.guild_id}`);
	const guildHref = $derived(`/applications/${rule.application_id}/guilds/${rule.guild_id}`);
	const listed = $derived(channelById(channels, rule.channel_id) !== null);
	const title = $derived(listed ? channelName(channels, rule.channel_id) : `Channel ${rule.channel_id}`);
	const crumbs = $derived([
		...(session.can('manage_watch_rules') ? [{ label: 'Watch rules', href: '/rules' }] : [{ label: 'Discord servers', href: '/guilds' }]),
		{ label: guildName, href: guildHref },
		{ label: title }
	]);
	const taken = $derived(takenChannels(data.guildRules, rule.id));

	let form = $state<RuleInput>(emptyRule());
	let formKey = $state(0);
	let formRef = $state<RuleForm | undefined>();
	$effect.pre(() => {
		form = toInput(rule);
		untrack(() => (formKey += 1));
	});
	const dirty = $derived(!sameInput(form, toInput(rule)));

	let saving = $state(false);
	let error = $state<string | null>(null);
	let deleting = $state(false);

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!formRef?.valid()) {
			error = 'Check the fields below.';
			return;
		}
		saving = true;
		error = null;
		try {
			await rules.update(rule.id, cleanInput(form));
			toast.ok('Rule saved.');
			await invalidate(`app:rule:${rule.id}`);
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			saving = false;
		}
	}

	function reset() {
		form = toInput(rule);
		formKey += 1;
		error = null;
	}

	// The profile assigned to the channel: its own, else the server's, else the server's.

	const inForce = $derived(inForceFor(data.assignments, rule.channel_id));
	const own = $derived(inForce?.scope.kind === 'channel');
	const channelScope = $derived<Scope>({ kind: 'channel', guild_id: rule.guild_id, channel_id: rule.channel_id });
	let profileChoice = $state('');
	$effect(() => {
		profileChoice = inForce?.profile_id ?? '';
	});
	let assigning = $state(false);

	async function putInForce() {
		if (!profileChoice) return;
		assigning = true;
		try {
			await profilesApi.assign(channelScope, profileChoice);
			toast.ok(`${profileName(data.profiles, profileChoice)} assigned to ${title}.`);
			await invalidate(`app:rule:${rule.id}`);
		} catch (cause) {
			toast.error(`Could not assign the profile: ${messageOf(cause)}`);
		} finally {
			assigning = false;
		}
	}

	async function takeOff() {
		assigning = true;
		try {
			await profilesApi.unassign(channelScope);
			toast.ok(`${title} follows the server's profile again.`);
			await invalidate(`app:rule:${rule.id}`);
		} catch (cause) {
			toast.error(`Could not remove the assignment: ${messageOf(cause)}`);
		} finally {
			assigning = false;
		}
	}

	async function remove() {
		const ok = await confirm.ask({
			title: `Stop watching ${title}?`,
			message: 'The rule is removed. Links posted there are no longer picked up.',
			confirmLabel: 'Remove rule',
			danger: true
		});
		if (!ok) return;
		deleting = true;
		try {
			await rules.remove(rule.id);
			toast.ok('Rule removed.');
			await goto(guildHref);
		} catch (cause) {
			toast.error(`Could not remove the rule: ${messageOf(cause)}`);
			deleting = false;
		}
	}
</script>

<svelte:head>
	<title>{title} · Watch rules · DiscoClip</title>
</svelte:head>

<PageHeader {title} {crumbs}>
	{#snippet meta()}
		{#if rule.enabled}<Badge tone="ok" size="sm" dot>Enabled</Badge>{:else}<Badge size="sm">Disabled</Badge>{/if}
		{#if listed}<code class="small">{rule.channel_id}</code>{/if}
		<span class="faint small">in <a href={guildHref}>{guildName}</a> · {appName}</span>
		{#if status}<BotBadge state={status.state} size="sm" />{/if}
	{/snippet}
	{#snippet actions()}
		<Button href={guildHref} icon="arrow-left">Server rules</Button>
	{/snippet}
</PageHeader>

<SectionNav items={[{ id: 'rule-settings', label: 'Rule' }, { id: 'rule-profile', label: 'Platforms and limits' }]} />

<div class="stack-lg">
	{#if status && status.state !== 'connected'}
		<Alert tone="info" message="Changing a rule goes through the bot, so the bot must be connected." />
	{/if}

	<section class="card" id="rule-settings" tabindex="-1">
		<div class="card-header">
			<h2>Rule</h2>
			<span class="faint small">Added <Time value={rule.created_at} /> · updated <Time value={rule.updated_at} /></span>
		</div>
		<form class="card-body stack" onsubmit={save} novalidate>
			{#if error}
				<FormFeedback message={error} />
			{/if}
			{#key formKey}
				<RuleForm id="rule" bind:value={form} bind:this={formRef} disabled={saving} directory={data.directory} {taken} />
			{/key}
			<div class="form-actions">
				<p class="hint" role="status">{dirty ? 'Unsaved changes' : 'All changes saved'}</p>
				<div class="row">
					<Button variant="ghost" onclick={reset} disabled={!dirty || saving}>Reset</Button>
					<Button type="submit" variant="primary" loading={saving} disabled={!dirty}>Save rule</Button>
				</div>
			</div>
		</form>
	</section>

	<section class="card" id="rule-profile" tabindex="-1">
		<div class="card-header">
			<div>
				<h2>Platforms and limits</h2>
				<p class="hint">Which platforms count in {title} and its media limits come from the assigned profile.</p>
			</div>
		</div>
		<div class="card-body stack-sm">
			{#if inForce}
				{@const profile = data.profiles.find((p) => p.id === inForce.profile_id) ?? null}
				<div class="row">
					<a class="strong" href={`/profiles/${inForce.profile_id}`}>{profileName(data.profiles, inForce.profile_id)}</a>
					<span class="faint small">
						{#if own}the channel's own{:else if inForce.scope.kind === 'guild'}the server's{:else}global default{/if}
						{#if profile}· {describeLimits(profile.limits, 'no limits of its own')}{/if}
					</span>
				</div>
				{#if profile?.description}<span class="muted small">{profile.description}</span>{/if}
				<div class="row">
					<Field label="Profile for this channel" for="control-7457"><select id="control-7457" class="select" bind:value={profileChoice} aria-label="Profile for this channel" disabled={assigning}>
						{#each data.profiles as profile (profile.id)}
							<option value={profile.id}>{profile.name}</option>
						{/each}
					</select></Field>
					<Button size="sm" variant="primary" loading={assigning} disabled={!profileChoice || (own && profileChoice === inForce.profile_id)} onclick={putInForce}>Assign profile</Button>
					{#if own}
						<Button size="sm" variant="ghost" icon="x" disabled={assigning} onclick={takeOff}>Use server profile</Button>
					{/if}
				</div>
			{:else}
				<span class="muted">The profiles assigned here are set from <a href={guildHref}>the server's page</a>.</span>
			{/if}
		</div>
	</section>

	<section class="card card-danger">
		<div class="card-header"><h2>Remove rule</h2></div>
		<div class="card-body row-between">
			<p class="muted">Stops watching {title} in {guildName}.</p>
			<Button variant="danger" icon="trash" loading={deleting} onclick={remove}>Remove rule</Button>
		</div>
	</section>
</div>
