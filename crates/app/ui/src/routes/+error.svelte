<script lang="ts">
	import { page } from '$app/state';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import { session } from '$lib/state/session.svelte';

	const status = $derived(page.status);
	const message = $derived(
		status === 404 && /^not found$/i.test(page.error?.message ?? '')
			? 'There is nothing at this address.'
			: (page.error?.message ?? 'Something went wrong.')
	);
	const title = $derived(
		status === 404
			? 'Page not found'
			: status === 403
				? 'Not allowed'
				: status === 503 || status === 502
					? 'Server unreachable'
					: status === 429
						? 'Slow down'
						: 'Something went wrong'
	);
	const icon = $derived(status === 403 ? 'lock' : status === 404 ? 'search' : 'warning');
</script>

<svelte:head>
	<title>{title} · DiscoClip</title>
</svelte:head>

<div class="error-page">
	<span class="glyph"><Icon name={icon} size={22} /></span>
	<p class="code">{status}</p>
	<h1>{title}</h1>
	<p class="muted">{message}</p>
	<div class="row">
		<Button variant="primary" href={session.me ? '/' : '/login'} icon="arrow-left">
			{session.me ? 'Back to the overview' : 'Go to login'}
		</Button>
		<Button variant="ghost" onclick={() => history.back()}>Go back</Button>
	</div>
</div>

<style>
	.error-page {
		min-height: 60vh;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		text-align: center;
		gap: 8px;
		padding: 40px 20px;
	}

	.glyph {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 48px;
		height: 48px;
		border-radius: 14px;
		background: var(--accent-soft);
		color: var(--accent-text);
		margin-bottom: 8px;
	}

	.code {
		font-family: var(--font-mono);
		color: var(--text-3);
		font-size: 12px;
		letter-spacing: 0.1em;
	}

	.error-page p.muted {
		max-width: 48ch;
	}

	.row {
		margin-top: 14px;
	}
</style>
