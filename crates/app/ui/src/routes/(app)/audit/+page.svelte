<script lang="ts">
	import ChevronDownIcon from '@lucide/svelte/icons/chevron-down';
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { audit, users as usersApi } from '$lib/api/endpoints';
	import type { Action, AuditQuery, Entry, TargetKind, User } from '$lib/api/types';
	import CodeBlock from '$lib/components/CodeBlock.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import RelativeTime from '$lib/components/RelativeTime.svelte';
	import Spinner from '$lib/components/Spinner.svelte';
	import Toolbar from '$lib/components/Toolbar.svelte';
	import { absolute, number } from '$lib/format';
	import { reportError } from '$lib/toast.svelte';

	const LIMIT = 50;
	const ACTIONS: Action[] = [
		'settings.set',
		'settings.reset',
		'settings.import',
		'settings.provision',
		'application.create',
		'application.update',
		'application.delete',
		'application.commands.set',
		'application.commands.register',
		'bot.start',
		'bot.stop',
		'bot.restart',
		'rule.create',
		'rule.update',
		'rule.delete',
		'session.import',
		'session.clear',
		'profile.create',
		'profile.update',
		'profile.delete',
		'profile.assign',
		'profile.unassign',
		'frontend.create',
		'frontend.update',
		'frontend.delete',
		'frontend.secret.set',
		'frontend.secret.clear',
		'frontend.user.create',
		'frontend.user.password',
		'frontend.user.delete',
		'frontend.sessions.revoke'
	];
	const TARGETS: TargetKind[] = [
		'setting',
		'application',
		'rule',
		'platform',
		'profile',
		'frontend'
	];

	let entries = $state<Entry[]>([]);
	let next = $state<string | null>(null);
	let users = $state<User[]>([]);
	let loading = $state(true);
	let loadingMore = $state(false);
	let error = $state<unknown>(null);
	let expanded = $state<string | null>(null);

	let actor = $state('');
	let action = $state<Action | ''>('');
	let targetKind = $state<TargetKind | ''>('');
	let targetId = $state('');
	let since = $state('');
	let until = $state('');

	function toIso(text: string): string | undefined {
		if (!text) return undefined;
		const d = new Date(text);
		return Number.isNaN(d.getTime()) ? undefined : d.toISOString();
	}

	function query(before?: string): AuditQuery {
		return {
			actor: actor || undefined,
			action: action || undefined,
			target_kind: targetKind || undefined,
			target_id: targetKind && targetId.trim() ? targetId.trim() : undefined,
			since: toIso(since),
			until: toIso(until),
			limit: LIMIT,
			before
		};
	}

	let requestId = 0;
	async function load() {
		const current = ++requestId;
		loading = true;
		error = null;
		try {
			const page = await audit.list(query());
			if (current !== requestId) return;
			entries = page.entries;
			next = page.next;
		} catch (err) {
			if (current !== requestId) return;
			error = err;
		} finally {
			if (current === requestId) loading = false;
		}
	}

	async function more() {
		if (!next || loadingMore) return;
		loadingMore = true;
		try {
			const page = await audit.list(query(next));
			entries = [...entries, ...page.entries];
			next = page.next;
		} catch (err) {
			reportError(err, 'Could not load more');
		} finally {
			loadingMore = false;
		}
	}

	onMount(() => {
		void load();
		usersApi
			.list()
			.then((list) => (users = list))
			.catch(() => (users = []));
	});

	function clear() {
		actor = '';
		action = '';
		targetKind = '';
		targetId = '';
		since = '';
		until = '';
		void load();
	}

	function targetHref(entry: Entry): string | null {
		const { kind, id } = entry.target;
		switch (kind) {
			case 'application':
				return resolve('/(app)/applications/[id]', { id });
			case 'profile':
				return resolve('/(app)/profiles/[id]', { id });
			case 'frontend':
				return resolve('/(app)/views/[id]', { id });
			case 'platform':
				return resolve('/(app)/platforms/[id]', { id });
			case 'rule': {
				const app = entry.details.application_id;
				const guild = entry.details.guild_id;
				return typeof app === 'string' && typeof guild === 'string'
					? resolve('/(app)/applications/[id]/guilds/[guild]', { id: app, guild })
					: null;
			}
			case 'setting':
				return null;
		}
	}

	const pretty = (value: unknown) => (value === undefined ? '' : JSON.stringify(value, null, 2));

	function otherDetails(entry: Entry): [string, unknown][] {
		return Object.entries(entry.details).filter(([k]) => k !== 'value' && k !== 'previous');
	}
</script>

<PageHeader title="Audit log" />

<form
	class="grid gap-3 card border border-surface-200-800 bg-surface-100-900 p-5 sm:p-6 md:grid-cols-3"
	onsubmit={(event) => {
		event.preventDefault();
		void load();
	}}
>
	<select class="select" bind:value={actor} aria-label="Actor">
		<option value="">Any actor</option>
		{#each users as user (user.id)}<option value={user.id}>{user.username}</option>{/each}
	</select>
	<select class="select" bind:value={action} aria-label="Action">
		<option value="">Any action</option>
		{#each ACTIONS as a (a)}<option value={a}>{a}</option>{/each}
	</select>
	<div class="flex gap-2">
		<select class="select" bind:value={targetKind} aria-label="Target kind">
			<option value="">Any target</option>
			{#each TARGETS as t (t)}<option value={t}>{t}</option>{/each}
		</select>
		<input
			class="input font-mono"
			type="text"
			placeholder="Target id"
			aria-label="Target id"
			bind:value={targetId}
			disabled={!targetKind}
		/>
	</div>
	<input class="input" type="datetime-local" aria-label="Since" bind:value={since} />
	<input class="input" type="datetime-local" aria-label="Until" bind:value={until} />
	<div class="flex justify-end gap-2">
		<button type="button" class="btn preset-tonal" onclick={clear}>Clear</button>
		<button type="submit" class="btn preset-filled">Apply</button>
	</div>
</form>

<Toolbar
	description="Who changed settings, applications, rules, profiles and views. Secrets are redacted."
/>

{#if error && !loading}
	<ErrorState {error} onretry={load} />
{:else if loading && entries.length === 0}
	<div class="space-y-2" aria-busy="true">
		{#each { length: 6 }, i (i)}<div class="h-12 placeholder animate-pulse"></div>{/each}
	</div>
{:else if entries.length === 0}
	<EmptyState title="No entries" description="Nothing matches these filters." />
{:else}
	<ul
		class="divide-y divide-surface-200-800 rounded-container border border-surface-200-800 bg-surface-100-900"
	>
		{#each entries as entry (entry.id)}
			{@const href = targetHref(entry)}
			{@const open = expanded === entry.id}
			<li>
				<button
					type="button"
					class="grid w-full grid-cols-[auto_1fr_auto] items-center gap-3 px-3 py-2 text-left text-sm hover:bg-surface-200-800"
					onclick={() => (expanded = open ? null : entry.id)}
					aria-expanded={open}
				>
					<ChevronDownIcon
						class="size-4 text-surface-600-400 transition {open ? 'rotate-180' : ''}"
					/>
					<span class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
						<span class="font-medium">
							{#if entry.actor.kind === 'user'}{entry.actor.username}{:else}Provisioning{/if}
						</span>
						<span class="font-mono text-sm">{entry.action}</span>
						<span class="truncate text-surface-600-400">
							{entry.target.kind} · {entry.target.name ?? entry.target.id}
						</span>
					</span>
					<RelativeTime at={entry.at} class="text-sm whitespace-nowrap text-surface-600-400" />
				</button>
				{#if open}
					<div
						class="space-y-3 border-t border-surface-200-800 bg-surface-50-950 px-4 py-3 text-sm"
					>
						<dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
							<dt class="text-surface-600-400">When</dt>
							<dd>{absolute(entry.at)}</dd>
							<dt class="text-surface-600-400">Actor</dt>
							<dd>
								{#if entry.actor.kind === 'user'}
									{entry.actor.username} via {entry.actor.via} from
									<span class="font-mono">{entry.actor.ip}</span>
								{:else}
									Provisioning{entry.actor.file ? ` file ${entry.actor.file}` : ' environment'}
								{/if}
							</dd>
							<dt class="text-surface-600-400">Target</dt>
							<dd>
								{entry.target.kind} <span class="font-mono">{entry.target.id}</span>
								{#if entry.target.name}· {entry.target.name}{/if}
								{#if href}· <a class="link-body underline" {href}>Open</a>{/if}
							</dd>
							<dt class="text-surface-600-400">Entry</dt>
							<dd class="font-mono">{entry.id}</dd>
						</dl>
						{#if 'value' in entry.details || 'previous' in entry.details}
							<div class="grid gap-3 md:grid-cols-2">
								<div>
									<p class="mb-1 text-sm font-medium text-surface-600-400">Previous</p>
									<CodeBlock code={pretty(entry.details.previous)} wrap />
								</div>
								<div>
									<p class="mb-1 text-sm font-medium text-surface-600-400">Value</p>
									<CodeBlock code={pretty(entry.details.value)} wrap />
								</div>
							</div>
						{/if}
						{#if otherDetails(entry).length > 0}
							<div>
								<p class="mb-1 text-sm font-medium text-surface-600-400">Details</p>
								<CodeBlock code={pretty(Object.fromEntries(otherDetails(entry)))} wrap />
							</div>
						{/if}
					</div>
				{/if}
			</li>
		{/each}
	</ul>
	<div class="flex items-center justify-between text-sm text-surface-600-400">
		<span>{number(entries.length)} entries shown</span>
		{#if next}
			<button type="button" class="btn preset-tonal" onclick={more} disabled={loadingMore}>
				{#if loadingMore}<Spinner />{/if}
				Load older
			</button>
		{/if}
	</div>
{/if}
