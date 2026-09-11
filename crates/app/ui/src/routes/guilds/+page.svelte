<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { guilds, isApiError, messageOf, providers } from '$lib/api';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize, shortId } from '$lib/format';
	import { bots } from '$lib/state/bots.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	let refreshing = $state(false);
	const canRulesAnywhere = $derived(session.can('manage_watch_rules'));
	const manageable = $derived(data.guilds.filter((g) => g.manageable || canRulesAnywhere));
	const others = $derived(data.guilds.filter((g) => !(g.manageable || canRulesAnywhere)));
	const fetchedAt = $derived(data.guilds[0]?.fetched_at ?? null);

	function botName(id: string): string {
		const status = bots.status(id);
		return status?.state === 'connected' ? status.user : `Bot ${shortId(id)}`;
	}

	async function refresh() {
		refreshing = true;
		try {
			const list = await guilds.refresh();
			toast.ok(`Fetched ${pluralize(list.length, 'guild')} from Discord.`);
			await invalidate('app:guilds');
		} catch (cause) {
			if (isApiError(cause) && cause.status === 404) {
				toast.error('Link your Discord account first; there is no Discord grant to fetch guilds with.');
			} else {
				toast.error(`Could not fetch your guilds: ${messageOf(cause)}`);
			}
		} finally {
			refreshing = false;
		}
	}
</script>

<svelte:head>
	<title>My guilds · DiscoClip</title>
</svelte:head>

<PageHeader title="My guilds" description="The Discord guilds of your linked Discord account. Managing a guild there lets you edit its watch rules here.">
	{#snippet actions()}
		{#if data.discordLinked}
			<Button icon="refresh" loading={refreshing} onclick={refresh}>Fetch again</Button>
		{/if}
	{/snippet}
</PageHeader>

{#if !data.discordLinked}
	<Empty icon="discord" title="No Discord account linked" description={data.discordOffered ? 'Link your Discord account to see your guilds and edit the rules of the ones you manage.' : 'Discord login is not offered on this server yet. An admin marks a Discord application for login from its page.'}>
		{#if data.discordOffered}
			<Button variant="primary" icon="link" href={providers.startUrl('discord', 'link')} external>Link Discord</Button>
		{/if}
		<Button href="/account#logins">Linked logins</Button>
	</Empty>
{:else if data.guilds.length === 0}
	<Empty icon="server" title="No guilds stored yet" description="Fetch them from Discord to see the guilds your account belongs to.">
		<Button variant="primary" icon="refresh" loading={refreshing} onclick={refresh}>Fetch guilds</Button>
	</Empty>
{:else}
	<div class="stack-lg">
		{#if fetchedAt}
			<p class="faint small">As fetched <Time value={fetchedAt} />. {bots.ids.length === 0 ? 'No bot is running, so there are no rules to edit yet.' : ''}</p>
		{/if}

		{#if manageable.length}
			<section class="stack">
				<div class="section-title">
					<h2>Guilds you manage</h2>
					<span class="faint small">{pluralize(manageable.length, 'guild')}</span>
				</div>
				<div class="grid-3">
					{#each manageable as guild (guild.id)}
						<article class="card guild">
							<div class="guild-head">
								<GuildIcon id={guild.id} icon={guild.icon} name={guild.name} size={40} />
								<div class="guild-title">
									<span class="strong">{guild.name}</span>
									<code class="small">{guild.id}</code>
								</div>
							</div>
							<div class="chips">
								{#if guild.owner}<Badge tone="accent" size="sm"><Icon name="crown" size={12} /> Owner</Badge>{/if}
								{#if guild.manageable}<Badge tone="ok" size="sm">Manages server</Badge>{:else}<Badge size="sm">Member</Badge>{/if}
							</div>
							<div class="bots">
								{#each bots.ids as id (id)}
									<a class="bot-link" href={`/applications/${id}/guilds/${guild.id}`}>
										<Icon name="bot" size={15} />
										<span class="truncate">{botName(id)}</span>
										<BotBadge state={bots.statuses[id]!.state} size="sm" />
										<span class="rules">Rules <Icon name="chevron-right" size={13} /></span>
									</a>
								{:else}
									<p class="faint small">No bot is running yet.</p>
								{/each}
							</div>
						</article>
					{/each}
				</div>
			</section>
		{/if}

		{#if others.length}
			<section class="stack">
				<div class="section-title">
					<h2>Other guilds</h2>
					<span class="faint small">You are a member but do not manage these on Discord.</span>
				</div>
				<div class="table-wrap">
					<table class="table">
						<thead><tr><th>Guild</th><th>Id</th><th>Role</th></tr></thead>
						<tbody>
							{#each others as guild (guild.id)}
								<tr>
									<td><div class="row"><GuildIcon id={guild.id} icon={guild.icon} name={guild.name} size={26} /><span>{guild.name}</span></div></td>
									<td><code>{guild.id}</code></td>
									<td class="faint">Member</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</section>
		{/if}
	</div>
{/if}

<style>
	.guild {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px 18px;
	}

	.guild-head {
		display: flex;
		align-items: center;
		gap: 12px;
	}

	.guild-title {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.guild-title .strong {
		overflow-wrap: anywhere;
	}

	.bots {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
		padding-top: 10px;
		border-top: 1px solid var(--border);
	}

	.bot-link {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 8px;
		margin: 0 -8px;
		border-radius: var(--radius-sm);
		color: inherit;
		text-decoration: none;
	}

	.bot-link:hover {
		background: var(--surface-3);
		text-decoration: none;
	}

	.rules {
		margin-left: auto;
		display: inline-flex;
		align-items: center;
		gap: 2px;
		color: var(--accent-text);
		font-size: 12.5px;
		font-weight: 500;
	}
</style>
