<script lang="ts">
	import { tick } from 'svelte';

	let { message }: { message: string } = $props();
	let element = $state<HTMLDivElement>();
	let fields = $state<{ id: string; label: string; error: string }[]>([]);
	const uid = $props.id();

	$effect(() => {
		void message;
		void tick().then(() => {
			if (!element?.isConnected) return;
			const form = element.closest('form');
			fields = Array.from(form?.querySelectorAll<HTMLElement>('[aria-invalid="true"][id]') ?? [])
				.map((control) => ({
					id: control.id,
					label: control.closest('.field')?.querySelector('label')?.textContent?.replace(/\s+/g, ' ').trim() ?? 'Field',
					error: control.closest('.field')?.querySelector('.error-text')?.textContent?.trim() ?? ''
				}));
			element.focus();
		});
	});

	function focusField(event: MouseEvent, id: string) {
		event.preventDefault();
		const control = document.getElementById(id);
		let details = control?.closest('details');
		while (details) { details.open = true; details = details.parentElement?.closest('details') ?? null; }
		control?.scrollIntoView({ block: 'center' });
		control?.focus({ preventScroll: true });
	}
</script>

<div class="feedback" bind:this={element} role="alert" tabindex="-1" aria-labelledby={`${uid}-title`}>
	<h2 id={`${uid}-title`}>{fields.length ? 'Check these fields' : 'There is a problem'}</h2>
	{#if fields.length}
		<ul>
			{#each fields as field (field.id)}
				<li><a href={`#${field.id}`} onclick={(event) => focusField(event, field.id)}>{field.label}{field.error ? `: ${field.error}` : ''}</a></li>
			{/each}
		</ul>
	{:else}
		<p>{message}</p>
	{/if}
</div>

<style>
	.feedback { border: 2px solid var(--danger); border-radius: var(--radius-sm); padding: 20px; background: var(--surface); }
	h2 { font-size: 16px; margin-bottom: 8px; }
	ul { margin: 0; padding-left: 20px; }
	a { color: var(--danger-text); text-decoration: underline; }
	li + li { margin-top: 8px; }
</style>
