<script lang="ts">
	import Field from '$lib/components/Field.svelte';
	import { invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { messageOf, profiles as api } from '$lib/api';
	import type { Profile } from '$lib/api';
	import Badge from '$lib/components/Badge.svelte';
	import Button from '$lib/components/Button.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Time from '$lib/components/Time.svelte';
	import { pluralize } from '$lib/format';
	import { describeLimits, describePlatforms, inForceCount, profileName } from '$lib/profiles';
	import { confirm } from '$lib/state/confirm.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	let { data }: { data: PageData } = $props();

	const canEdit = $derived(session.can('manage_settings'));
	const globalId = $derived(data.global?.profile_id ?? null);
	const refresh = () => invalidate('app:profiles');

	// Global default's profile

	let globalChoice = $state('');
	let savingGlobal = $state(false);
	$effect(() => {
		globalChoice = globalId ?? '';
	});

	async function saveGlobal() {
		if (!globalChoice || globalChoice === globalId) return;
		savingGlobal = true;
		try {
			await api.assign({ kind: 'global' }, globalChoice);
			toast.ok(`${profileName(data.profiles, globalChoice)} assigned to the whole server.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not change the server's profile: ${messageOf(cause)}`);
		} finally {
			savingGlobal = false;
		}
	}

	// Remove

	let deleting = $state<string | null>(null);

	async function remove(profile: Profile) {
		const where = inForceCount(data.assignments, profile.id);
		const ok = await confirm.ask({
			title: `Remove the profile ${profile.name}?`,
			message: where.total
				? `It is assigned to ${where.text}. These assignments will use their inherited profiles.`
				: 'This profile has no assignments.',
			confirmLabel: 'Remove profile',
			danger: true
		});
		if (!ok) return;
		deleting = profile.id;
		try {
			await api.remove(profile.id);
			toast.ok(`Profile ${profile.name} removed.`);
			await refresh();
		} catch (cause) {
			toast.error(`Could not remove the profile: ${messageOf(cause)}`);
		} finally {
			deleting = null;
		}
	}

	let query = $state('');
	const visible = $derived.by(() => {
		const needle = query.trim().toLowerCase();
		if (!needle) return data.profiles;
		return data.profiles.filter(
			(p) => p.name.toLowerCase().includes(needle) || p.description.toLowerCase().includes(needle)
		);
	});
</script>

<svelte:head>
	<title>Profiles · DiscoClip</title>
</svelte:head>

<PageHeader
	title="Profiles"
	description="Control platform access and media limits."
>
	{#snippet actions()}
		{#if canEdit}
			<Button variant="primary" icon="plus" href="/profiles/new">New profile</Button>
		{/if}
	{/snippet}
</PageHeader>

<div class="stack-lg">
	<section class="card">
		<div class="card-header">
			<div>
				<h2>Global default</h2>
				<p class="hint">Applies to web submissions and Discord servers without a custom profile.</p>
			</div>
		</div>
		<div class="card-body">
			{#if canEdit}
				<div class="row">
					<Field label="Profile for the whole server" for="control-3217"><select id="control-3217" class="select global" bind:value={globalChoice} aria-label="Profile for the whole server" disabled={savingGlobal}>
						{#each data.profiles as profile (profile.id)}
							<option value={profile.id}>{profile.name}</option>
						{/each}
					</select></Field>
					<Button variant="primary" loading={savingGlobal} disabled={!globalChoice || globalChoice === globalId} onclick={saveGlobal}>Assign profile</Button>
					{#if globalId}
						<span class="faint small">
							now <a href={`/profiles/${globalId}`}>{profileName(data.profiles, globalId)}</a>{#if data.global}, since <Time value={data.global.updated_at} />{/if}
						</span>
					{/if}
				</div>
			{:else if globalId}
				<span class="row">
					<a class="strong" href={`/profiles/${globalId}`}>{profileName(data.profiles, globalId)}</a>
					{#if data.global}<span class="faint small">since <Time value={data.global.updated_at} /></span>{/if}
				</span>
			{/if}
		</div>
	</section>

	<section class="card">
		<div class="card-header">
			<div>
				<h2>Profiles</h2>
				<p class="hint">Assign profiles from a Discord server page.</p>
			</div>
			<div class="row">
				<span class="faint small">{pluralize(data.profiles.length, 'profile')}</span>
				{#if data.profiles.length > 6}
					<Field label="Find a profile" for="control-4474"><input id="control-4474" class="input search" type="search" placeholder="Find a profile" bind:value={query} aria-label="Find a profile" /></Field>
				{/if}
			</div>
		</div>
		{#if visible.length === 0}
			<div class="card-body">
				<Empty compact icon={query ? 'search' : 'sparkles'} title={query ? 'No profile matches' : 'No profiles'} description={query ? 'No profile name or description contains that text.' : 'The server has no profiles.'} />
			</div>
		{:else}
			<div class="table-wrap flush">
				<table class="table">
					<thead>
						<tr>
							<th>Profile</th>
							<th>Platforms</th>
							<th>Limits</th>
							{#if data.assignments}<th>Assigned to</th>{/if}
							<th>Updated</th>
							<th></th>
						</tr>
					</thead>
					<tbody>
						{#each visible as profile (profile.id)}
							{@const where = inForceCount(data.assignments, profile.id)}
							<tr>
								<td>
									<div class="stack-sm" style="gap:2px">
										<span class="row">
											<a class="strong" href={`/profiles/${profile.id}`}>{profile.name}</a>
											{#if profile.builtin}<Badge size="sm" tone="info">Built in</Badge>{/if}
											{#if profile.id === globalId}<Badge size="sm" tone="ok" dot>Global default</Badge>{/if}
										</span>
										{#if profile.description}<span class="muted small">{profile.description}</span>{/if}
									</div>
								</td>
								<td class="small">{describePlatforms(profile.platforms, data.platforms, data.presets)}</td>
								<td class="small">{describeLimits(profile.limits, 'Inherited')}</td>
								{#if data.assignments}
									<td class="small">{#if where.total}{where.text}{:else}<span class="faint">nowhere</span>{/if}</td>
								{/if}
								<td><Time value={profile.updated_at} /></td>
								<td class="actions">
									<Button size="sm" variant="ghost" icon={canEdit ? 'pencil' : 'eye'} href={`/profiles/${profile.id}`}>{canEdit ? 'Edit' : 'View'}</Button>
									{#if canEdit && !profile.builtin}
										<Button size="sm" variant="ghost" icon="trash" loading={deleting === profile.id} disabled={profile.id === globalId} title={profile.id === globalId ? 'Choose another default profile before deleting this one.' : 'Remove profile'} onclick={() => remove(profile)} square />
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</section>
</div>

<style>
	.flush {
		border: none;
		border-radius: 0 0 var(--radius) var(--radius);
		box-shadow: none;
	}

	.global {
		width: 260px;
		flex: none;
	}

	.search {
		width: 220px;
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 4px;
	}
</style>
