<script lang="ts">
	import type { PageData } from './$types';
	import type { GuildChannel, Rule } from '$lib/api';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import GuildIcon from '$lib/components/GuildIcon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { channelName, groupKey } from '$lib/discord';
	import { pluralize, shortId } from '$lib/format';
	import { describeFilters, describeLimits } from '$lib/rules';
	import { bots } from '$lib/state/bots.svelte';
	import { session } from '$lib/state/session.svelte';

	let { data }: { data: PageData } = $props();

	let query = $state('');

	interface Group {
		key: string;
		applicationId: string;
		guildId: string;
		appName: string;
		guildName: string;
		guildIcon: string | null;
		/** The guild's channels, or `null` with `channelsError` when the bot could not list them. */
		channels: GuildChannel[] | null;
		channelsError: string | null;
		rules: Rule[];
	}

	function appName(id: string): string {
		const app = data.apps?.find((a) => a.id === id);
		if (app) return app.name;
		const status = bots.status(id);
		if (status?.state === 'connected') return status.user;
		return `Application ${shortId(id)}`;
	}

	function guildOf(applicationId: string, guildId: string) {
		return data.guildsByApp.get(applicationId)?.find((g) => g.guild_id === guildId) ?? null;
	}

	const groups = $derived.by((): Group[] => {
		const needle = query.trim().toLowerCase();
		const map = new Map<string, Group>();
		for (const rule of data.rules) {
			const key = groupKey(rule.application_id, rule.guild_id);
			let group = map.get(key);
			if (!group) {
				const guild = guildOf(rule.application_id, rule.guild_id);
				group = {
					key,
					applicationId: rule.application_id,
					guildId: rule.guild_id,
					appName: appName(rule.application_id),
					guildName: guild?.name ?? `Guild ${rule.guild_id}`,
					guildIcon: guild?.icon ?? null,
					channels: data.channelsByGuild.get(key) ?? null,
					channelsError: data.channelErrors.get(key) ?? null,
					rules: []
				};
				map.set(key, group);
			}
			group.rules.push(rule);
		}
		const all = [...map.values()].sort(
			(a, b) => a.appName.localeCompare(b.appName) || a.guildName.localeCompare(b.guildName)
		);
		if (!needle) return all;
		return all
			.map((group) => {
				const whole =
					group.appName.toLowerCase().includes(needle) ||
					group.guildName.toLowerCase().includes(needle) ||
					group.guildId.includes(needle) ||
					group.applicationId.includes(needle);
				const rules = whole
					? group.rules
					: group.rules.filter(
							(r) =>
								r.channel_id.includes(needle) ||
								channelName(group.channels, r.channel_id).toLowerCase().includes(needle) ||
								(r.post_to ?? '').includes(needle) ||
								r.allow_hosts.some((h) => h.includes(needle))
						);
				return { ...group, rules };
			})
			.filter((group) => group.rules.length > 0);
	});
</script>

<svelte:head>
	<title>Watch rules · DiscoClip</title>
</svelte:head>

{#snippet channelRef(channels: GuildChannel[] | null, id: string, href?: string)}
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

<PageHeader title="Watch rules" description="Every channel a bot watches, by application and guild.">
	{#snippet actions()}
		{#if session.can('manage_applications')}
			<Button href="/applications" icon="bot">Applications</Button>
		{/if}
	{/snippet}
</PageHeader>

{#if data.rules.length === 0}
	<Empty icon="rules" title="No watch rules yet" description="Rules are added per guild from an application's guild list, or from your guilds page for guilds you manage on Discord.">
		{#if session.can('manage_applications')}
			<Button variant="primary" href="/applications">Go to applications</Button>
		{/if}
		<Button href="/guilds">My guilds</Button>
	</Empty>
{:else}
	<div class="stack">
		<div class="row-between">
			<input class="input search" type="search" placeholder="Filter by guild, application, channel or host" bind:value={query} aria-label="Filter rules" />
			<span class="faint small">{pluralize(data.rules.length, 'rule')} in {pluralize(groups.length, 'guild')}</span>
		</div>

		{#each groups as group (group.key)}
			{@const status = bots.status(group.applicationId)}
			<section class="card">
				<div class="card-header">
					<div class="row">
						<GuildIcon id={group.guildId} icon={group.guildIcon} name={group.guildName} size={30} />
						<div class="stack-sm" style="gap:0">
							<a href={`/applications/${group.applicationId}/guilds/${group.guildId}`} class="strong">{group.guildName}</a>
							<span class="faint small row">
								{group.appName}
								{#if status}<BotBadge state={status.state} size="sm" />{/if}
								<code>{group.guildId}</code>
							</span>
						</div>
					</div>
					<Button size="sm" variant="ghost" href={`/applications/${group.applicationId}/guilds/${group.guildId}`} iconRight="chevron-right">Manage</Button>
				</div>
				{#if group.channelsError}
					<p class="hint listing">Shown by channel id: the bot could not list the guild's channels ({group.channelsError}).</p>
				{/if}
				<div class="table-wrap flush">
					<table class="table">
						<thead>
							<tr><th>Channel</th><th>Posts to</th><th>Who and where from</th><th>Limits</th><th>Status</th><th>Updated</th><th></th></tr>
						</thead>
						<tbody>
							{#each group.rules as rule (rule.id)}
								<tr class={[!rule.enabled && 'off']}>
									<td>{@render channelRef(group.channels, rule.channel_id, `/rules/${rule.id}`)}</td>
									<td>{#if rule.post_to}{@render channelRef(group.channels, rule.post_to)}{:else}<span class="faint">same channel</span>{/if}</td>
									<td>{describeFilters(rule)}</td>
									<td>{describeLimits(rule)}</td>
									<td>{#if rule.enabled}<Badge tone="ok" size="sm" dot>Enabled</Badge>{:else}<Badge size="sm">Disabled</Badge>{/if}</td>
									<td><Time value={rule.updated_at} /></td>
									<td class="actions"><Button size="sm" variant="ghost" href={`/rules/${rule.id}`} icon="pencil">Edit</Button></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</section>
		{:else}
			<Empty compact icon="search" title="Nothing matches" description="No rule matches that filter." />
		{/each}
	</div>
{/if}

<style>
	.search {
		max-width: 380px;
	}

	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.listing {
		padding: 10px 20px 0;
	}

	tr.off td {
		color: var(--text-3);
	}
</style>
