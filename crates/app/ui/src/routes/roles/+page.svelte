<script lang="ts">
	import type { PageData } from './$types';
	import { PERMISSIONS } from '$lib/api';
	import Avatar from '$lib/components/Avatar.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import { pluralize } from '$lib/format';
	import { PERMISSION_LABELS, ROLE_LABELS } from '$lib/permissions';
	import { session } from '$lib/state/session.svelte';

	let { data }: { data: PageData } = $props();
</script>

<svelte:head>
	<title>Roles · DiscoClip</title>
</svelte:head>

<PageHeader title="Roles" description="What each role allows, and who holds it. A role is given to an account from its page; an API token gets only what its account's role allows.">
	{#snippet actions()}
		<Button href="/users" icon="users">Accounts</Button>
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-header"><h2>Permissions by role</h2></div>
		<div class="table-wrap flush">
			<table class="table matrix">
				<thead>
					<tr>
						<th>Permission</th>
						{#each data.roles as role (role.role)}
							<th class="center">{ROLE_LABELS[role.role].label}</th>
						{/each}
					</tr>
				</thead>
				<tbody>
					{#each PERMISSIONS as permission (permission)}
						<tr>
							<td>
								<span class="strong">{PERMISSION_LABELS[permission].label}</span>
								<span class="faint small block">{PERMISSION_LABELS[permission].description}</span>
								<code class="small">{permission}</code>
							</td>
							{#each data.roles as role (role.role)}
								{@const allowed = role.permissions.includes(permission)}
								<td class="center">
									<span class={['mark', allowed ? 'yes' : 'no']} title={allowed ? 'Allowed' : 'Not allowed'}>
										<Icon name={allowed ? 'check' : 'x'} size={14} />
									</span>
								</td>
							{/each}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>

	<div class="grid-3">
		{#each data.roles as role (role.role)}
			<section class="card role">
				<div class="role-head">
					<div>
						<h2>{ROLE_LABELS[role.role].label}</h2>
						<p class="hint">{role.description}</p>
					</div>
					<Badge tone={role.role === 'admin' ? 'accent' : 'neutral'}>{pluralize(role.accounts.length, 'account')}</Badge>
				</div>
				{#if role.accounts.length === 0}
					<p class="faint small">Nobody holds this role.</p>
				{:else}
					<ul class="accounts">
						{#each role.accounts as user (user.id)}
							<li>
								<a href={`/users/${user.id}`} class="row row-link">
									<Avatar name={user.username} size={26} />
									<span>{user.username}</span>
									{#if user.id === session.user?.id}<Badge tone="info" size="sm">You</Badge>{/if}
									{#if !user.has_password}<Badge tone="warn" size="sm">No password</Badge>{/if}
								</a>
							</li>
						{/each}
					</ul>
				{/if}
			</section>
		{/each}
	</div>
</div>

<style>
	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.matrix th.center,
	.matrix td.center {
		text-align: center;
	}

	.block {
		display: block;
	}

	.mark {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		border-radius: 50%;
	}

	.mark.yes {
		background: var(--ok-soft);
		color: var(--ok-text);
	}

	.mark.no {
		background: var(--surface-3);
		color: var(--text-3);
	}

	.role {
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 16px 18px;
	}

	.role-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
	}

	.accounts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
</style>
