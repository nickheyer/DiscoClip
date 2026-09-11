<script lang="ts">
	import { goto, invalidate } from '$app/navigation';
	import type { PageData } from './$types';
	import { applications, messageOf } from '$lib/api';
	import Alert from '$lib/components/Alert.svelte';
	import Badge from '$lib/components/Badge.svelte';
	import BotBadge from '$lib/components/BotBadge.svelte';
	import BotControls from '$lib/components/BotControls.svelte';
	import Button from '$lib/components/Button.svelte';
	import CopyButton from '$lib/components/CopyButton.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import Empty from '$lib/components/Empty.svelte';
	import Field from '$lib/components/Field.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import PasswordInput from '$lib/components/PasswordInput.svelte';
	import Time from '$lib/components/Time.svelte';
	import { bots } from '$lib/state/bots.svelte';
	import { toast } from '$lib/state/toast.svelte';
	import { APPLICATION_NAME_MAX } from '$lib/validation';

	let { data }: { data: PageData } = $props();

	let dialog = $state(false);
	let botToken = $state('');
	let name = $state('');
	let clientSecret = $state('');
	let creating = $state(false);
	let error = $state<string | null>(null);

	const nameProblem = $derived(
		name.trim().length > APPLICATION_NAME_MAX ? `A name is up to ${APPLICATION_NAME_MAX} characters.` : null
	);
	const ready = $derived(botToken.trim() !== '' && !nameProblem);

	function open() {
		botToken = '';
		name = '';
		clientSecret = '';
		error = null;
		dialog = true;
	}

	async function create(event: SubmitEvent) {
		event.preventDefault();
		if (!ready) return;
		creating = true;
		error = null;
		try {
			const app = await applications.create({
				bot_token: botToken.trim(),
				name: name.trim() || undefined,
				client_secret: clientSecret.trim() || undefined
			});
			bots.put(app.id, app.bot);
			dialog = false;
			toast.ok(`Added ${app.name}. Its bot is starting.`);
			await invalidate('app:applications');
			await goto(`/applications/${app.id}`);
		} catch (cause) {
			error = messageOf(cause);
		} finally {
			creating = false;
		}
	}

	function commandsLabel(mode: string, guilds: string[]): string {
		if (mode === 'global') return 'Global commands';
		if (mode === 'guilds') return `Commands in ${guilds.length} guild${guilds.length === 1 ? '' : 's'}`;
		return 'Commands off';
	}
</script>

<svelte:head>
	<title>Applications · DiscoClip</title>
</svelte:head>

<PageHeader title="Applications" description="Each Discord application runs its own bot. Add one with its bot token.">
	{#snippet actions()}
		<Button variant="primary" icon="plus" onclick={open}>Add application</Button>
	{/snippet}
</PageHeader>

{#if data.apps.length === 0}
	<Empty
		icon="bot"
		title="No applications yet"
		description="Create an application in the Discord Developer Portal, copy its bot token, and add it here. DiscoClip stores the token encrypted and runs the bot."
	>
		<Button variant="primary" icon="plus" onclick={open}>Add application</Button>
		<Button href="https://discord.com/developers/applications" newTab icon="external">Developer Portal</Button>
	</Empty>
{:else}
	<div class="grid-3">
		{#each data.apps as app (app.id)}
			{@const status = bots.status(app.id) ?? app.bot}
			<article class="card app">
				<div class="app-head">
					<div class="app-title">
						<a href={`/applications/${app.id}`} class="strong name">{app.name}</a>
						<span class="row faint small">
							<code>{app.client_id}</code>
							<CopyButton text={app.client_id} square label="Copy client id" />
						</span>
					</div>
					<BotBadge state={status.state} />
				</div>
				<div class="chips">
					{#if status.state === 'connected'}<Badge tone="ok" size="sm">@{status.user}</Badge>{/if}
					{#if !app.enabled}<Badge tone="neutral" size="sm">Meant to stay stopped</Badge>{/if}
					{#if app.login}<Badge tone="accent" size="sm">Offers login</Badge>{/if}
					<Badge size="sm" tone={app.commands.error ? 'danger' : 'neutral'}>{commandsLabel(app.commands.mode, app.commands.guilds)}</Badge>
				</div>
				{#if status.state === 'failed' || status.state === 'retrying'}
					<Alert tone={status.state === 'failed' ? 'danger' : 'warn'} message={status.error} />
				{/if}
				<p class="faint small">Updated <Time value={app.updated_at} /></p>
				<div class="app-foot">
					<BotControls application={app.id} botState={status.state} />
					<Button size="sm" variant="ghost" href={`/applications/${app.id}`} iconRight="chevron-right">Manage</Button>
				</div>
			</article>
		{/each}
	</div>
{/if}

<Dialog bind:open={dialog} title="Add a Discord application" busy={creating}>
	<form id="app-form" class="stack" onsubmit={create} novalidate>
		{#if error}
			<Alert tone="danger" message={error} onclose={() => (error = null)} />
		{/if}
		<Field label="Bot token" for="app-token" hint="Developer Portal → your application → Bot → Reset Token. Stored encrypted; the bot starts with it right away.">
			<PasswordInput id="app-token" bind:value={botToken} mono required />
		</Field>
		<Field label="Name" for="app-name" optional hint="How the application is shown here. Defaults to its name on Discord." error={nameProblem}>
			<input id="app-name" class="input" bind:value={name} maxlength={APPLICATION_NAME_MAX} autocomplete="off" aria-invalid={nameProblem ? 'true' : undefined} />
		</Field>
		<Field label="Client secret" for="app-secret" optional hint="Developer Portal → OAuth2 → Client Secret. Needed only to let people log in to DiscoClip with Discord through this application.">
			<PasswordInput id="app-secret" bind:value={clientSecret} mono />
		</Field>
	</form>
	{#snippet footer()}
		<Button variant="ghost" onclick={() => (dialog = false)} disabled={creating}>Cancel</Button>
		<Button variant="primary" loading={creating} disabled={!ready} onclick={() => document.querySelector<HTMLFormElement>('#app-form')?.requestSubmit()}>Add and start bot</Button>
	{/snippet}
</Dialog>

<style>
	.app {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 16px 18px;
	}

	.app-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
	}

	.app-title {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}

	.name {
		font-size: 15px;
		overflow-wrap: anywhere;
	}

	.app-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		flex-wrap: wrap;
		margin-top: auto;
		padding-top: 6px;
	}
</style>
