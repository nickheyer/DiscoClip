<script lang="ts">
	let { items, label = 'Sections' }: {
		items: { id: string; label: string }[];
		label?: string;
	} = $props();

	function jump(event: MouseEvent, id: string) {
		const target = document.getElementById(id);
		if (!target) return;
		event.preventDefault();
		target.scrollIntoView({ block: 'start' });
		target.focus({ preventScroll: true });
	}
</script>

<nav class="section-nav" aria-label={label}>
	{#each items as item (item.id)}
		<a href={`#${item.id}`} onclick={(event) => jump(event, item.id)}>{item.label}</a>
	{/each}
</nav>

<style>
	.section-nav {
		position: sticky;
		top: 0;
		z-index: 10;
		display: flex;
		gap: 4px;
		flex-wrap: wrap;
		padding: 8px 0;
		margin-bottom: 24px;
		background: var(--bg);
		border-bottom: 1px solid var(--border-strong);
	}

	a {
		display: flex;
		align-items: center;
		min-height: 44px;
		padding: 8px 14px;
		border-radius: var(--radius-sm);
		font-weight: 500;
		color: var(--text-2);
	}

	a:hover, a:focus-visible {
		background: var(--accent-soft);
		color: var(--accent-text);
		text-decoration: none;
	}

	@media (max-width: 900px) {
		.section-nav { top: 60px; }
	}

	@media (max-width: 600px) {
		.section-nav { position: static; }
	}
</style>
