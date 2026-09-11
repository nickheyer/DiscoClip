<script lang="ts">
	let { label = 'Loading', inline = false }: { label?: string; inline?: boolean } = $props();
</script>

<div class={['loading', inline && 'inline']} role="status" aria-live="polite">
	<span class="spinner" aria-hidden="true"></span>
	<span>{label}…</span>
</div>

<style>
	.loading {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 10px;
		padding: 40px 0;
		color: var(--text-3);
	}

	.inline {
		padding: 0;
		justify-content: flex-start;
	}

	.spinner {
		width: 16px;
		height: 16px;
		border-radius: 50%;
		border: 2px solid var(--border-strong);
		border-top-color: var(--accent);
		animation: spin 0.8s linear infinite;
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
</style>
