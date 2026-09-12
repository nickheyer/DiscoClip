<script lang="ts">
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, users } from '$lib/api';
	import type { AccountTokenView } from '$lib/api';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { PERMISSION_LABELS } from '$lib/permissions';
	import { confirm } from '$lib/state/confirm.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	let query = $state('');
	const shown = $derived.by(() => {
		const needle = query.trim().toLowerCase();
		if (!needle) return data.tokens;
		return data.tokens.filter(
			(t) =>
				t.username.toLowerCase().includes(needle) ||
				t.name.toLowerCase().includes(needle) ||
				t.prefix.includes(needle) ||
				t.scopes.some((s) => s.includes(needle))
		);
	});

	let revoking = $state<string | null>(null);

	async function revoke(token: AccountTokenView) {
		const ok = await confirm.ask({
			title: `Revoke “${token.name}” of ${token.username}?`,
			message: 'Anything still using this token stops working at once.',
			confirmLabel: 'Revoke',
			danger: true
		});
		if (!ok) return;
		revoking = token.id;
		try {
			await users.revokeToken(token.user_id, token.id);
			toast.ok(`Revoked ${token.name}.`);
			await invalidate('app:tokens');
		} catch (cause) {
			toast.error(`Could not revoke the token: ${messageOf(cause)}`);
		} finally {
			revoking = null;
		}
	}
</script>

<svelte:head>
	<title>API tokens · DiscoClip</title>
</svelte:head>

<PageHeader title="API tokens" description="Every live token of every account. Accounts mint their own from their account page; each does only what it was given, within its account's role.">
	{#snippet actions()}
		<Button href="/account#tokens" icon="key">Your tokens</Button>
	{/snippet}
</PageHeader>

<div class="stack">
	<div class="row-between">
		<input class="input search" type="search" placeholder="Filter by account, name, prefix or scope" bind:value={query} aria-label="Filter tokens" />
		<span class="faint small">{pluralize(shown.length, 'token')}</span>
	</div>

	{#if shown.length === 0}
		<Empty icon="key" title={query ? 'Nothing matches' : 'No API tokens'} description={query ? 'No token matches that filter.' : 'Nobody has minted a token yet.'} />
	{:else}
		<div class="table-wrap">
			<table class="table">
				<thead>
					<tr>
						<th>Account</th>
						<th>Name</th>
						<th>Prefix</th>
						<th>Scopes</th>
						<th>Created</th>
						<th>Last used</th>
						<th>Expires</th>
						<th></th>
					</tr>
				</thead>
				<tbody>
					{#each shown as token (token.id)}
						<tr>
							<td>
								<a href={`/users/${token.user_id}`} class="row row-link">
									<Avatar name={token.username} size={26} />
									<span>{token.username}</span>
								</a>
							</td>
							<td class="strong">{token.name}</td>
							<td><code>{token.prefix}…</code></td>
							<td>
								{#if token.scopes.length === 0}
									<span class="faint">none</span>
								{:else}
									<div class="chips">
										{#each token.scopes as scope (scope)}
											<Badge size="sm">{PERMISSION_LABELS[scope].label}</Badge>
										{/each}
									</div>
								{/if}
							</td>
							<td><Time value={token.created_at} /></td>
							<td><Time value={token.last_used_at} empty="never" /></td>
							<td><Time value={token.expires_at} empty="never" /></td>
							<td class="actions">
								<Button size="sm" variant="ghost" loading={revoking === token.id} onclick={() => revoke(token)}>Revoke</Button>
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
