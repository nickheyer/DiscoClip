<script lang="ts">
	import Button from './Button.svelte';
	import Dialog from './Dialog.svelte';
	import { confirm } from '$lib/state/confirm.svelte';

	let typed = $state('');
	const pending = $derived(confirm.pending);
	const open = $derived(pending !== null);
	const ready = $derived(!pending?.typed || typed.trim() === pending.typed);

	$effect(() => {
		if (pending) typed = '';
	});

	function submit(event: SubmitEvent) {
		event.preventDefault();
		if (ready) confirm.answer(true);
	}
</script>

{#if pending}
	<Dialog
		{open}
		title={pending.title}
		size="sm"
		onclose={() => confirm.answer(false)}
	>
		<form id="confirm-form" class="stack" onsubmit={submit}>
			<p class="muted">{pending.message}</p>
			{#if pending.typed}
				<label class="stack-sm">
					<span class="small">Type <code>{pending.typed}</code> to confirm</span>
					<!-- svelte-ignore a11y_autofocus -->
					<input class="input mono" bind:value={typed} autocomplete="off" autofocus />
				</label>
			{/if}
		</form>
		{#snippet footer()}
			<Button variant="ghost" onclick={() => confirm.answer(false)}>
				{pending.cancelLabel ?? 'Cancel'}
			</Button>
			<Button
				variant={pending.danger ? 'danger' : 'primary'}
				type="submit"
				disabled={!ready}
				onclick={() => confirm.answer(true)}
			>
				{pending.confirmLabel ?? 'Confirm'}
			</Button>
		{/snippet}
	</Dialog>
{/if}
