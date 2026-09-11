<script lang="ts">
	import Button from './Button.svelte';
	import { applications, messageOf } from '$lib/api';
	import type { ApplicationView, BotStateName } from '$lib/api';
	import { bots } from '$lib/state/bots.svelte';
	import { session } from '$lib/state/session.svelte';
	import { toast } from '$lib/state/toast.svelte';

	interface Props {
		application: string;
		botState: BotStateName;
		size?: 'sm' | 'md';
		/** Receives the application as the API returns it after each action. */
		onchange?: (view: ApplicationView) => void;
	}

	let { application, botState, size = 'sm', onchange }: Props = $props();

	let busy = $state<'start' | 'stop' | 'restart' | null>(null);
	const allowed = $derived(session.can('manage_bots'));
	const running = $derived(botState === 'starting' || botState === 'connected' || botState === 'retrying');

	async function run(action: 'start' | 'stop' | 'restart') {
		busy = action;
		try {
			const call =
				action === 'start'
					? applications.startBot
					: action === 'stop'
						? applications.stopBot
						: applications.restartBot;
			const view = await call(application);
			bots.put(view.id, view.bot);
			onchange?.(view);
			toast.ok(
				action === 'start'
					? `Starting ${view.name}`
					: action === 'stop'
						? `Stopped ${view.name}`
						: `Restarting ${view.name}`
			);
		} catch (error) {
			toast.error(`Could not ${action} the bot: ${messageOf(error)}`);
		} finally {
			busy = null;
		}
	}
</script>

{#if allowed}
	<div class="row" role="group" aria-label="Bot controls">
		{#if running}
			<Button {size} icon="stop" loading={busy === 'stop'} disabled={busy !== null} onclick={() => run('stop')}>
				Stop
			</Button>
			<Button {size} icon="restart" loading={busy === 'restart'} disabled={busy !== null} onclick={() => run('restart')}>
				Restart
			</Button>
		{:else}
			<Button
				{size}
				variant="primary"
				icon="play"
				loading={busy === 'start'}
				disabled={busy !== null || botState === 'disabled'}
				title={botState === 'disabled' ? 'This application has no bot token' : undefined}
				onclick={() => run('start')}
			>
				Start
			</Button>
		{/if}
	</div>
{/if}
