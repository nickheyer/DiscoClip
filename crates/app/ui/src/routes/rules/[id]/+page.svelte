<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, rules } from '$lib/api';
	import type { RuleInput } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RuleForm from '$lib/components/RuleForm.svelte';
	import Time from '$lib/components/Time.svelte';
	import { channelById, channelName, takenChannels } from '$lib/discord';
	import { shortId } from '$lib/format';
	import { untrack } from 'svelte';
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
	const guildName = $derived(data.botGuild?.name ?? `Guild ${rule.guild_id}`);
	const guildHref = $derived(`/applications/${rule.application_id}/guilds/${rule.guild_id}`);
	const listed = $derived(channelById(channels, rule.channel_id) !== null);
	const title = $derived(listed ? channelName(channels, rule.channel_id) : `Channel ${rule.channel_id}`);
	const crumbs = $derived([
		...(session.can('manage_watch_rules') ? [{ label: 'Watch rules', href: '/rules' }] : [{ label: 'My guilds', href: '/guilds' }]),
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
			error = 'Fix the highlighted fields first.';
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
		<Button href={guildHref} icon="arrow-left">All rules of the guild</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	{#if status && status.state !== 'connected'}
		<Alert tone="info" message="Changing a rule goes through the bot, so the bot must be connected." />
	{/if}

	<section class="card">
		<div class="card-header">
			<h2>Rule</h2>
			<span class="faint small">Added <Time value={rule.created_at} /> · updated <Time value={rule.updated_at} /></span>
		</div>
		<form class="card-body stack" onsubmit={save} novalidate>
			{#if error}
				<Alert tone="danger" message={error} onclose={() => (error = null)} />
			{/if}
			{#key formKey}
				<RuleForm id="rule" bind:value={form} bind:this={formRef} disabled={saving} directory={data.directory} {taken} />
			{/key}
			<div class="row-between">
				<p class="hint">Saving checks the channels through the bot.</p>
				<div class="row">
					<Button variant="ghost" onclick={reset} disabled={!dirty || saving}>Reset</Button>
					<Button type="submit" variant="primary" loading={saving} disabled={!dirty}>Save rule</Button>
				</div>
			</div>
		</form>
	</section>

	<section class="card card-danger">
		<div class="card-header"><h2>Remove rule</h2></div>
		<div class="card-body row-between">
			<p class="muted">Stops watching {title} in {guildName}.</p>
			<Button variant="danger" icon="trash" loading={deleting} onclick={remove}>Remove rule</Button>
		</div>
	</section>
</div>
