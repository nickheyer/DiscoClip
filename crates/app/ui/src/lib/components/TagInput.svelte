<script lang="ts">
	import Icon from './Icon.svelte';

	interface Props {
		id: string;
		values: string[];
		placeholder?: string;
		/** A message when a value is not acceptable; nothing when it is. */
		validate?: (value: string) => string | null;
		normalize?: (value: string) => string;
		mono?: boolean;
		disabled?: boolean;
		/** Known values offered as a datalist. */
		suggestions?: { value: string; label: string }[];
	}

	let {
		id,
		values = $bindable([]),
		placeholder = 'Type and press Enter',
		validate,
		normalize = (v) => v.trim(),
		mono = true,
		disabled = false,
		suggestions = []
	}: Props = $props();

	let draft = $state('');
	let problem = $state<string | null>(null);
	let input: HTMLInputElement | undefined = $state();

	const listId = $derived(`${id}-suggestions`);

	function commit(raw: string): boolean {
		const value = normalize(raw);
		if (!value) return false;
		const message = validate?.(value) ?? null;
		if (message) {
			problem = message;
			return false;
		}
		if (!values.includes(value)) values = [...values, value];
		problem = null;
		return true;
	}

	function commitDraft() {
		if (commit(draft)) draft = '';
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' || event.key === ',') {
			event.preventDefault();
			commitDraft();
		} else if (event.key === 'Backspace' && draft === '' && values.length) {
			values = values.slice(0, -1);
		}
	}

	function onPaste(event: ClipboardEvent) {
		const text = event.clipboardData?.getData('text') ?? '';
		if (!/[\s,]/.test(text.trim())) return;
		event.preventDefault();
		for (const part of text.split(/[\s,]+/)) commit(part);
	}

	function remove(value: string) {
		values = values.filter((v) => v !== value);
		input?.focus();
	}

	function labelOf(value: string): string {
		const hit = suggestions.find((s) => s.value === value);
		return hit && hit.label !== value ? `${hit.label} · ${value}` : value;
	}
</script>

<div class={['tags', disabled && 'disabled']} role="group">
	{#each values as value (value)}
		<span class={['tag', mono && 'mono']}>
			<span class="truncate">{labelOf(value)}</span>
			{#if !disabled}
				<button type="button" class="remove" onclick={() => remove(value)} aria-label={`Remove ${value}`}>
					<Icon name="x" size={12} />
				</button>
			{/if}
		</span>
	{/each}
	<input
		{id}
		bind:this={input}
		bind:value={draft}
		class={['draft', mono && 'mono']}
		{placeholder}
		{disabled}
		list={suggestions.length ? listId : undefined}
		autocomplete="off"
		spellcheck="false"
		onkeydown={onKeydown}
		onpaste={onPaste}
		onblur={commitDraft}
		oninput={() => (problem = null)}
		aria-invalid={problem ? 'true' : undefined}
		aria-describedby={problem ? `${id}-problem` : undefined}
	/>
	{#if suggestions.length}
		<datalist id={listId}>
			{#each suggestions as s (s.value)}
				<option value={s.value}>{s.label}</option>
			{/each}
		</datalist>
	{/if}
</div>
{#if problem}
	<p class="error-text" id={`${id}-problem`} role="alert">{problem}</p>
{/if}

<style>
	.tags {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
		min-height: 36px;
		padding: 4px 6px;
		border: 1px solid var(--border-strong);
		border-radius: var(--radius-sm);
		background: var(--surface);
		cursor: text;
	}

	.tags:focus-within {
		border-color: var(--accent);
		box-shadow: var(--focus);
	}

	.tags.disabled {
		background: var(--surface-2);
		cursor: not-allowed;
	}

	.tag {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		max-width: 100%;
		padding: 2px 4px 2px 8px;
		border-radius: 6px;
		background: var(--accent-soft);
		color: var(--accent-text);
		font-size: 12.5px;
		line-height: 1.5;
	}

	.remove {
		display: flex;
		padding: 2px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: inherit;
		cursor: pointer;
		opacity: 0.7;
	}

	.remove:hover {
		opacity: 1;
		background: color-mix(in srgb, currentColor 15%, transparent);
	}

	.draft {
		flex: 1;
		min-width: 140px;
		border: none;
		background: transparent;
		padding: 4px;
		outline: none;
	}

	.draft::placeholder {
		color: var(--text-3);
	}
</style>
