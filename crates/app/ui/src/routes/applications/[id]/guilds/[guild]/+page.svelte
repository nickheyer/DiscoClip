<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, rules } from '$lib/api';
	import type { Rule, RuleInput } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RuleForm from '$lib/components/RuleForm.svelte';
	import Time from '$lib/components/Time.svelte';
	import { shortId } from '$lib/format';
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

	// Add and edit

	let dialog = $state(false);
	let editing = $state<Rule | null>(null);
	let form = $state<RuleInput>(emptyRule());
	let formRef = $state<RuleForm | undefined>();
	let saving = $state(false);
	let error = $state<string | null>(null);

	function openAdd() {
		editing = null;
		form = emptyRule();
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
		try {
			if (editing) {
				await rules.update(editing.id, input);
				toast.ok(`Rule for channel ${input.channel_id} saved.`);
			} else {
				await rules.createForGuild(data.applicationId, data.guildId, input);
				toast.ok(`Now watching channel ${input.channel_id}.`);
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
		toggling = rule.id;
		try {
			await rules.update(rule.id, { ...toInput(rule), enabled: !rule.enabled });
			toast.ok(rule.enabled ? `Channel ${rule.channel_id} is no longer watched.` : `Channel ${rule.channel_id} is watched again.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not change the rule: ${messageOf(cause)}`);
		} finally {
			toggling = null;
		}
	}

	async function remove(rule: Rule) {
		const ok = await confirm.ask({
			title: `Stop watching channel ${rule.channel_id}?`,
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
		<Button variant="primary" icon="plus" onclick={openAdd}>Add rule</Button>
	{/snippet}
</PageHeader>

<div class="stack">
	{#if data.botGuild && !present}
		<Alert tone="warn" title="The bot was removed from this guild" message="Rules stay, but nothing is watched until the bot is added again." />
	{:else if !data.botGuild && data.app}
		<Alert tone="info" title="The bot has not joined this guild" message="Use the install link to add it. Rules can be prepared now." />
	{/if}
	{#if status && status.state !== 'connected'}
		<Alert tone="info" message="Adding or changing a rule checks the channel through the bot, so the bot must be connected." />
	{/if}

	{#if data.rules.length === 0}
		<Empty icon="rules" title="No rules for this guild" description="A rule names a channel to watch, where results go, whose links count and how big a video may be.">
			<Button variant="primary" icon="plus" onclick={openAdd}>Add the first rule</Button>
		</Empty>
	{:else}
		<div class="table-wrap">
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
							<td><a href={`/rules/${rule.id}`} class="row-link"><code>{rule.channel_id}</code></a></td>
							<td>{#if rule.post_to}<code>{rule.post_to}</code>{:else}<span class="faint">same channel</span>{/if}</td>
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
</div>

<Dialog bind:open={dialog} title={editing ? `Edit rule for channel ${editing.channel_id}` : 'Add a watch rule'} size="lg" busy={saving}>
	<form id="rule-form" class="stack" onsubmit={save} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<RuleForm id="rule" bind:value={form} bind:this={formRef} disabled={saving} />
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={saving}>Cancel</Button>
		<Button variant="primary" loading={saving} onclick={() => document.querySelector<HTMLFormElement>('#rule-form')?.requestSubmit()}>{editing ? 'Save rule' : 'Add rule'}</Button>
	{/snippet}
</Dialog>

<style>
	tr.off td {
		color: var(--text-3);
	}
</style>
