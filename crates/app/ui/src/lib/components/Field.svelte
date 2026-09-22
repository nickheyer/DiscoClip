<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		label: string;
		for?: string;
		hint?: string;
		error?: string | null;
		optional?: boolean;
		children: Snippet;
	}

	let { label, for: htmlFor, hint, error = null, optional = false, children }: Props = $props();
	const uid = $props.id();

	function associate(node: HTMLDivElement, options: { target?: string; hint?: string; error: string | null }) {
		const owned = new Set([`${uid}-hint`, `${uid}-error`]);
		let invalidControl: HTMLElement | null = null;
		function update() {
			const control = options.target
				? Array.from(node.querySelectorAll<HTMLElement>('[id]')).find((element) => element.id === options.target)
				: node.querySelector<HTMLElement>('input, select, textarea, [role="combobox"]');
			if (!control) return;
			const ids = (control.getAttribute('aria-describedby') ?? '').split(' ').filter((id) => id && !owned.has(id));
			if (options.hint) ids.push(`${uid}-hint`);
			if (options.error) ids.push(`${uid}-error`);
			if (ids.length) control.setAttribute('aria-describedby', ids.join(' '));
			else control.removeAttribute('aria-describedby');
			if (!control.matches('input, select, textarea, button')) {
				control.setAttribute('role', 'group');
				control.setAttribute('aria-labelledby', `${uid}-label`);
			}
			if (options.error) {
				control.setAttribute('aria-invalid', 'true');
				invalidControl = control;
			} else if (invalidControl === control) {
				control.removeAttribute('aria-invalid');
				invalidControl = null;
			}
		}
		update();
		const observer = new MutationObserver(update);
		observer.observe(node, { childList: true, subtree: true });
		return {
			update(next: typeof options) { options = next; update(); },
			destroy() { observer.disconnect(); }
		};
	}
</script>

<div class={['field', error && 'has-error']} use:associate={{ target: htmlFor, hint, error }}>
	<label class="label" id={`${uid}-label`} for={htmlFor}>
		{label}
		{#if optional}<span class="optional">(optional)</span>{/if}
	</label>
	{#if hint}<p id={`${uid}-hint`} class="hint">{hint}</p>{/if}
	{@render children()}
	{#if error}
		<p id={`${uid}-error`} class="error-text">{error}</p>
	{/if}
</div>

<style>
	.field {
		display: flex;
		flex-direction: column;
		gap: 8px;
		min-width: 0;
	}

	.label {
		display: flex;
		align-items: baseline;
		gap: 8px;
		font-size: 14px;
		font-weight: 600;
		color: var(--text);
	}

	.optional {
		font-size: 13px;
		font-weight: 400;
		color: var(--text-3);
	}

	.has-error {
		border-left: 3px solid var(--danger);
		padding-left: 12px;
	}
</style>
