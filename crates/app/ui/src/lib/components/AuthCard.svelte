<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	let { title, subtitle, children }: { title: string; subtitle?: string; children: Snippet } =
		$props();
</script>

<div class="auth">
	<div class="card auth-card">
		<div class="brand">
			<span class="logo"><Icon name="video" size={18} /></span>
			<span class="name">DiscoClip</span>
		</div>
		<h1>{title}</h1>
		{#if subtitle}<p class="muted">{subtitle}</p>{/if}
		<div class="content">{@render children()}</div>
	</div>
</div>

<style>
	.auth {
		min-height: 100vh;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
		background:
			radial-gradient(60% 50% at 50% 0%, color-mix(in srgb, var(--accent) 14%, transparent), transparent 70%),
			var(--bg);
	}

	.auth-card {
		width: 100%;
		max-width: 400px;
		padding: 28px 28px 24px;
		display: flex;
		flex-direction: column;
		gap: 6px;
		box-shadow: var(--shadow);
	}

	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-bottom: 14px;
		font-weight: 600;
		font-size: 15px;
	}

	.logo {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		border-radius: 9px;
		background: linear-gradient(135deg, var(--accent), color-mix(in srgb, var(--accent) 60%, #22d3ee));
		color: #fff;
		box-shadow: var(--shadow-sm);
	}

	h1 {
		font-size: 20px;
	}

	.content {
		margin-top: 18px;
	}
</style>
