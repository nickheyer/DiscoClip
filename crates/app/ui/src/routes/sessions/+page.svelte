<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, users } from '$lib/api';
	import type { AccountSessionView } from '$lib/api';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { describeUserAgent, pluralize } from '$lib/format';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	let query = $state('');
	const shown = $derived.by(() => {
		const needle = query.trim().toLowerCase();
		if (!needle) return data.sessions;
		return data.sessions.filter(
			(s) =>
				s.username.toLowerCase().includes(needle) ||
				(s.ip ?? '').includes(needle) ||
				describeUserAgent(s.user_agent).toLowerCase().includes(needle)
		);
	});
	const accounts = $derived(new Set(data.sessions.map((s) => s.user_id)).size);

	let ending = $state<string | null>(null);

	async function end(view: AccountSessionView) {
		if (view.current) {
			const ok = await confirm.ask({
				title: 'End your own session?',
				message: 'This is the session you are using now. Ending it logs you out here.',
				confirmLabel: 'Log out',
				danger: true
			});
			if (!ok) return;
		}
		ending = view.id;
		try {
			await users.revokeSession(view.user_id, view.id);
			if (view.current) {
				await session.leave();
				return;
			}
			toast.ok(`Ended a session of ${view.username}.`);
			await invalidate('app:sessions');
		} catch (cause) {
			toast.error(`Could not end the session: ${messageOf(cause)}`);
		} finally {
			ending = null;
		}
	}
</script>

<svelte:head>
	<title>Sessions · DiscoClip</title>
</svelte:head>

<PageHeader title="Sessions" description="Every live browser session of every account. Ending one logs that browser out at once.">
	{#snippet actions()}
		<Button href="/users" icon="users">Accounts</Button>
	{/snippet}
</PageHeader>

<div class="stack">
	<div class="row-between">
		<input class="input search" type="search" placeholder="Filter by account, address or client" bind:value={query} aria-label="Filter sessions" />
		<span class="faint small">{pluralize(shown.length, 'session')} across {pluralize(accounts, 'account')}</span>
	</div>

	{#if shown.length === 0}
		<Empty icon="laptop" title={query ? 'Nothing matches' : 'No live sessions'} description={query ? 'No session matches that filter.' : 'Nobody is logged in.'} />
	{:else}
		<div class="table-wrap">
			<table class="table">
				<thead>
					<tr>
						<th>Account</th>
						<th>Client</th>
						<th>Address</th>
						<th>Started</th>
						<th>Last seen</th>
						<th>Expires</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each shown as view (view.id)}
						<tr>
							<td>
								<a href={`/users/${view.user_id}`} class="row row-link">
									<Avatar name={view.username} size={26} />
									<span>{view.username}</span>
								</a>
							</td>
							<td>
								<div class="row">
									<Icon name={/Android|iPhone|iPad|Mobile/.test(view.user_agent ?? '') ? 'phone' : 'laptop'} size={15} />
									<span title={view.user_agent ?? undefined}>{describeUserAgent(view.user_agent)}</span>
									{#if view.current}<Badge tone="ok" size="sm">This device</Badge>{/if}
								</div>
							</td>
							<td class="mono">{view.ip ?? '—'}</td>
							<td><Time value={view.created_at} /></td>
							<td><Time value={view.last_seen_at} /></td>
							<td><Time value={view.expires_at} /></td>
							<td class="actions">
								<Button size="sm" variant="ghost" loading={ending === view.id} onclick={() => end(view)}>
									{view.current ? 'Log out' : 'End'}
								</Button>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</div>

<style>
	.search {
		max-width: 380px;
	}
</style>
