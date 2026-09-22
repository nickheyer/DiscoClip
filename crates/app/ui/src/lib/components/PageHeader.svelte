<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	interface Crumb {
		label: string;
		href?: string;
	}

	interface Props {
		title: string;
		description?: string;
		crumbs?: Crumb[];
		actions?: Snippet;
		meta?: Snippet;
	}

	let { title, description, crumbs = [], actions, meta }: Props = $props();
</script>

<header class="page-header">
	{#if crumbs.length}
		<nav class="crumbs" aria-label="Breadcrumb">
			{#each crumbs as crumb, i (i)}
				{#if crumb.href}
					<a href={crumb.href}>{crumb.label}</a>
				{:else}
					<span>{crumb.label}</span>
				{/if}
				{#if i < crumbs.length - 1}
					<Icon name="chevron-right" size={12} />
				{/if}
			{/each}
		</nav>
	{/if}
	<div class="line">
		<div class="text">
			<h1>{title}</h1>
			{#if description}<p class="muted">{description}</p>{/if}
			{#if meta}<div class="meta">{@render meta()}</div>{/if}
		</div>
		{#if actions}<div class="actions">{@render actions()}</div>{/if}
	</div>
</header>

<style>
	.page-header {
		display: flex;
		flex-direction: column;
		gap: 6px;
		margin-bottom: 28px;
		padding-bottom: 24px;
		border-bottom: 1px solid var(--border);
	}

	.crumbs {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px;
		font-size: 13px;
		color: var(--text-3);
		overflow-wrap: anywhere;
	}

	.crumbs a {
		color: var(--text-2);
	}

	.line {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 16px;
		flex-wrap: wrap;
	}

	.text {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}

	.text p { max-width: 64ch; }

	.text h1 {
		overflow-wrap: anywhere;
	}

	.meta {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
		margin-top: 4px;
	}

	.actions {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
</style>
