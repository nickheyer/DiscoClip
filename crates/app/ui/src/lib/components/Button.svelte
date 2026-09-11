<script lang="ts">
	import type { Snippet } from 'svelte';
	import type { HTMLAnchorAttributes, HTMLButtonAttributes } from 'svelte/elements';
	import Icon from './Icon.svelte';

	type Variant = 'primary' | 'secondary' | 'ghost' | 'danger' | 'danger-soft' | 'link';
	type Size = 'sm' | 'md' | 'lg';

	interface Props {
		variant?: Variant;
		size?: Size;
		icon?: string;
		iconRight?: string;
		loading?: boolean;
		disabled?: boolean;
		href?: string;
		/** Leaves the app for the URL with a full page load. */
		external?: boolean;
		/** Opens the URL in a new tab. */
		newTab?: boolean;
		type?: 'button' | 'submit' | 'reset';
		title?: string;
		block?: boolean;
		square?: boolean;
		onclick?: (event: MouseEvent) => void;
		children?: Snippet;
		class?: string;
	}

	let {
		variant = 'secondary',
		size = 'md',
		icon,
		iconRight,
		loading = false,
		disabled = false,
		href,
		external = false,
		newTab = false,
		type = 'button',
		title,
		block = false,
		square = false,
		onclick,
		children,
		class: extra = ''
	}: Props = $props();

	const iconSize = $derived(size === 'sm' ? 14 : 16);
	const classes = $derived([
		'btn',
		`btn-${variant}`,
		`btn-${size}`,
		block && 'btn-block',
		square && 'btn-square',
		loading && 'btn-loading',
		extra
	]);
</script>

{#if href && !disabled}
	<a
		{href}
		class={classes}
		{title}
		target={newTab ? '_blank' : undefined}
		data-sveltekit-reload={external ? '' : undefined}
		rel={external || newTab ? 'noreferrer' : undefined}
		{onclick}
	>
		{#if icon}<Icon name={icon} size={iconSize} />{/if}
		{#if children}<span class="label">{@render children()}</span>{/if}
		{#if iconRight}<Icon name={iconRight} size={iconSize} />{/if}
	</a>
{:else}
	<button
		{type}
		class={classes}
		{title}
		disabled={disabled || loading}
		aria-busy={loading ? 'true' : undefined}
		{onclick}
	>
		{#if loading}
			<span class="spinner" aria-hidden="true"></span>
		{:else if icon}
			<Icon name={icon} size={iconSize} />
		{/if}
		{#if children}<span class="label">{@render children()}</span>{/if}
		{#if iconRight && !loading}<Icon name={iconRight} size={iconSize} />{/if}
	</button>
{/if}

<style>
	.btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 7px;
		min-height: 36px;
		padding: 0 14px;
		border: 1px solid transparent;
		border-radius: var(--radius-sm);
		font-weight: 500;
		font-size: 13.5px;
		line-height: 1;
		white-space: nowrap;
		cursor: pointer;
		text-decoration: none;
		user-select: none;
		transition:
			background-color 0.12s,
			border-color 0.12s,
			color 0.12s,
			box-shadow 0.12s,
			transform 0.06s;
	}

	.btn:active:not(:disabled) {
		transform: translateY(0.5px);
	}

	.btn:hover {
		text-decoration: none;
	}

	.btn:disabled {
		cursor: not-allowed;
		opacity: 0.55;
	}

	.btn-sm {
		min-height: 30px;
		padding: 0 10px;
		font-size: 12.5px;
		gap: 6px;
	}

	.btn-lg {
		min-height: 42px;
		padding: 0 18px;
		font-size: 14.5px;
	}

	.btn-block {
		width: 100%;
	}

	.btn-square {
		padding: 0;
		width: 36px;
	}

	.btn-square.btn-sm {
		width: 30px;
	}

	.btn-primary {
		background: var(--accent);
		color: var(--text-on-accent);
		box-shadow: var(--shadow-sm);
	}

	.btn-primary:hover:not(:disabled) {
		background: var(--accent-hover);
	}

	.btn-primary:active:not(:disabled) {
		background: var(--accent-active);
	}

	.btn-secondary {
		background: var(--surface);
		border-color: var(--border-strong);
		color: var(--text);
		box-shadow: var(--shadow-sm);
	}

	.btn-secondary:hover:not(:disabled) {
		background: var(--surface-2);
		border-color: color-mix(in srgb, var(--border-strong) 70%, var(--text-3));
	}

	.btn-ghost {
		background: transparent;
		color: var(--text-2);
	}

	.btn-ghost:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--text);
	}

	.btn-danger {
		background: var(--danger);
		color: #fff;
		box-shadow: var(--shadow-sm);
	}

	.btn-danger:hover:not(:disabled) {
		background: var(--danger-hover);
	}

	.btn-danger-soft {
		background: var(--surface);
		border-color: color-mix(in srgb, var(--danger) 45%, var(--border-strong));
		color: var(--danger-text);
	}

	.btn-danger-soft:hover:not(:disabled) {
		background: var(--danger-soft);
		border-color: var(--danger);
	}

	.btn-link {
		background: transparent;
		color: var(--accent-text);
		padding: 0;
		min-height: 0;
	}

	.btn-link:hover:not(:disabled) {
		text-decoration: underline;
	}

	.spinner {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		border: 2px solid currentColor;
		border-right-color: transparent;
		animation: spin 0.7s linear infinite;
		flex: none;
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
</style>
