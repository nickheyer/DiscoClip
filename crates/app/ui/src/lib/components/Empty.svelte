<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';

	interface Props {
		icon?: string;
		title: string;
		description?: string;
		compact?: boolean;
		children?: Snippet;
	}

	let { icon = 'info', title, description, compact = false, children }: Props = $props();
</script>

<div class={['empty', compact && 'compact']}>
	<span class="glyph"><Icon name={icon} size={compact ? 18 : 22} /></span>
	<h3>{title}</h3>
	{#if description}<p class="muted">{description}</p>{/if}
	{#if children}<div class="actions">{@render children()}</div>{/if}
</div>

<style>
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		text-align: center;
		gap: 6px;
		padding: 44px 24px;
		border: 1px dashed var(--border-strong);
		border-radius: var(--radius);
		background: var(--surface);
	}

	.compact {
		padding: 24px 16px;
	}

	.glyph {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 44px;
		height: 44px;
		border-radius: 12px;
		background: var(--accent-soft);
		color: var(--accent-text);
		margin-bottom: 6px;
	}

	.compact .glyph {
		width: 36px;
		height: 36px;
		border-radius: 10px;
	}

	.empty p {
		max-width: 44ch;
	}

	.actions {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		justify-content: center;
		margin-top: 10px;
	}
</style>
