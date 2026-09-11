<script lang="ts">
	import type { PageData } from './$types';
	import type { BotStatus } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import AuditEntry from '$lib/components/AuditEntry.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import BotControls from '$lib/components/BotControls.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { describeUserAgent, formatNumber, secondsUntil, shortId } from '$lib/format';
	import { ROLE_LABELS } from '$lib/permissions';
	import { bots } from '$lib/state/bots.svelte';
	import { clock } from '$lib/state/clock.svelte';
	import { session } from '$lib/state/session.svelte';

	let { data }: { data: PageData } = $props();

	const me = $derived(session.me!);
	const apps = $derived(data.apps?.ok ? data.apps.value : []);
	const appsById = $derived(new Map(apps.map((app) => [app.id, app])));
	const cards = $derived(
		bots.ids.map((id) => ({ id, status: bots.statuses[id]!, app: appsById.get(id) ?? null }))
	);
	const canManageApps = $derived(session.can('manage_applications'));
	const canViewAudit = $derived(session.can('view_audit_log'));

	interface Stat {
		label: string;
		icon: string;
		href: string;
		value: number | null;
		error: string | null;
	}

	const stats = $derived.by((): Stat[] => {
		const list: Stat[] = [];
		if (data.apps) {
			list.push({
				label: 'Applications',
				icon: 'bot',
				href: '/applications',
				value: data.apps.ok ? data.apps.value.length : null,
				error: data.apps.ok ? null : data.apps.error
			});
		}
		if (data.rules) {
			list.push({
				label: 'Watch rules',
				icon: 'rules',
				href: '/rules',
				value: data.rules.ok ? data.rules.value.length : null,
				error: data.rules.ok ? null : data.rules.error
			});
		}
		if (data.accounts) {
			list.push({
				label: 'Accounts',
				icon: 'users',
				href: '/users',
				value: data.accounts.ok ? data.accounts.value.length : null,
				error: data.accounts.ok ? null : data.accounts.error
			});
		}
		list.push({
			label: 'Your Discord guilds',
			icon: 'server',
			href: '/guilds',
			value: data.guilds.ok ? data.guilds.value.length : null,
			error: data.guilds.ok ? null : data.guilds.error
		});
		return list;
	});

	function title(id: string, status: BotStatus, name: string | undefined): string {
		if (name) return name;
		if (status.state === 'connected') return status.user;
		return `Bot ${shortId(id)}`;
	}
</script>

<svelte:head>
	<title>Overview · DiscoClip</title>
</svelte:head>

<PageHeader title="Overview" description="Every bot the server runs, live, and where to go next." />

<div class="stack-lg">
	<section>
		<div class="section-title">
			<h2>Bots</h2>
			<span class="faint small">
				{bots.state === 'live'
					? 'Updating live'
					: bots.state === 'reconnecting'
						? 'Reconnecting to the status stream…'
						: 'Connecting to the status stream…'}
			</span>
		</div>
		{#if cards.length === 0}
			<Empty
				icon="bot"
				title="No bots yet"
				description={canManageApps
					? 'Add a Discord application with its bot token and DiscoClip runs a bot for it.'
					: 'An admin adds Discord applications; their bots show up here as soon as they exist.'}
			>
				{#if canManageApps}
					<Button variant="primary" icon="plus" href="/applications">Add an application</Button>
				{/if}
			</Empty>
		{:else}
			<div class="grid-3">
				{#each cards as card (card.id)}
					<article class="card bot">
						<div class="bot-head">
							<div class="bot-title">
								{#if canManageApps}
									<a href={`/applications/${card.id}`} class="strong">{title(card.id, card.status, card.app?.name)}</a>
								{:else}
									<span class="strong">{title(card.id, card.status, card.app?.name)}</span>
								{/if}
								{#if card.status.state === 'connected'}
									<span class="faint small">as @{card.status.user}</span>
								{:else if card.app}
									<span class="faint small mono">{card.app.client_id}</span>
								{/if}
							</div>
							<BotBadge state={card.status.state} />
						</div>
						<p class="faint small">
							{#if card.status.state === 'connected'}Connected{:else if card.status.state === 'starting'}Starting{:else if card.status.state === 'retrying'}Retrying{:else if card.status.state === 'failed'}Failed{:else if card.status.state === 'stopped'}Stopped{:else}Without a token{/if}
							<Time value={card.status.since} />
						</p>
						{#if card.status.state === 'retrying'}
							<Alert tone="warn" title={`Attempt ${formatNumber(card.status.attempt)} · next try in ${secondsUntil(card.status.next_attempt_at, clock.now)}s`} message={card.status.error} />
						{:else if card.status.state === 'failed'}
							<Alert tone="danger" title="The bot stays down until it is started again" message={card.status.error} />
						{/if}
						<div class="bot-foot">
							<BotControls application={card.id} botState={card.status.state} />
							{#if canManageApps}
								<Button size="sm" variant="ghost" href={`/applications/${card.id}`} iconRight="chevron-right">Manage</Button>
							{/if}
						</div>
					</article>
				{/each}
			</div>
		{/if}
	</section>

	<section>
		<div class="grid-3">
			{#each stats as stat (stat.label)}
				<a class="card stat" href={stat.href}>
					<span class="stat-icon"><Icon name={stat.icon} size={18} /></span>
					<span class="stat-text">
						{#if stat.error}
							<span class="error-text">Could not load: {stat.error}</span>
						{:else}
							<span class="stat-value">{formatNumber(stat.value ?? 0)}</span>
						{/if}
						<span class="faint small">{stat.label}</span>
					</span>
					<Icon name="chevron-right" size={16} />
				</a>
			{/each}
		</div>
	</section>

	<div class="grid-2 lower">
		{#if canViewAudit && data.recent}
			<section class="card">
				<div class="card-header">
					<h2>Recent activity</h2>
					<Button size="sm" variant="ghost" href="/audit" iconRight="chevron-right">Audit log</Button>
				</div>
				{#if !data.recent.ok}
					<div class="card-body"><Alert tone="danger" message={`Could not load the audit log: ${data.recent.error}`} /></div>
				{:else if data.recent.value.entries.length === 0}
					<div class="card-body"><p class="muted">Nothing has been changed yet.</p></div>
				{:else}
					<div>
						{#each data.recent.value.entries as entry (entry.id)}
							<AuditEntry {entry} compact />
						{/each}
					</div>
				{/if}
			</section>
		{/if}

		<section class="card">
			<div class="card-header">
				<h2>Your session</h2>
				<Button size="sm" variant="ghost" href="/account" iconRight="chevron-right">Account</Button>
			</div>
			<div class="card-body">
				<dl class="kv">
					<dt>Account</dt>
					<dd class="row">
						<span class="strong">{me.user.username}</span>
						<Badge tone="accent" size="sm">{ROLE_LABELS[me.user.role].label}</Badge>
					</dd>
					{#if me.session}
						<dt>Address</dt>
						<dd class="mono">{me.session.ip ?? '—'}</dd>
						<dt>Client</dt>
						<dd title={me.session.user_agent ?? undefined}>{describeUserAgent(me.session.user_agent)}</dd>
						<dt>Logged in</dt>
						<dd><Time value={me.session.created_at} /></dd>
						<dt>Expires</dt>
						<dd><Time value={me.session.expires_at} /></dd>
					{/if}
				</dl>
			</div>
		</section>
	</div>
</div>

<style>
	.bot {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px 18px;
	}

	.bot-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
	}

	.bot-title {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.bot-title a,
	.bot-title .strong {
		overflow-wrap: anywhere;
	}

	.bot-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
		margin-top: auto;
		padding-top: 6px;
	}

	.stat {
		display: flex;
		align-items: center;
		gap: 14px;
		padding: 14px 16px;
		color: inherit;
		text-decoration: none;
		transition: border-color 0.12s;
	}

	.stat:hover {
		text-decoration: none;
		border-color: var(--border-strong);
	}

	.stat-icon {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 40px;
		height: 40px;
		border-radius: 11px;
		background: var(--accent-soft);
		color: var(--accent-text);
		flex: none;
	}

	.stat-text {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
		line-height: 1.25;
	}

	.stat-value {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.02em;
		font-variant-numeric: tabular-nums;
	}

	.lower {
		align-items: start;
	}
</style>
