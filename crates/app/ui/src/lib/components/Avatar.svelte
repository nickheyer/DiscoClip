<script lang="ts">
	import { initials } from '$lib/format';

	let { name, size = 32, src = null }: { name: string; size?: number; src?: string | null } =
		$props();

	let failed = $state(false);
	const style = $derived(`width:${size}px;height:${size}px;font-size:${Math.round(size * 0.38)}px`);
</script>

{#if src && !failed}
	<img class="avatar" {src} alt="" {style} onerror={() => (failed = true)} loading="lazy" />
{:else}
	<span class="avatar initials" {style} aria-hidden="true">{initials(name)}</span>
{/if}

<style>
	.avatar {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex: none;
		border-radius: 30%;
		object-fit: cover;
		background: var(--surface-3);
	}

	.initials {
		font-weight: 600;
		color: var(--text-2);
		letter-spacing: 0.02em;
		background: linear-gradient(135deg, var(--accent-soft), var(--surface-3));
	}
</style>
