<script lang="ts">
	interface Props {
		code: string;
		/** Wrap long lines instead of scrolling sideways. */
		wrap?: boolean;
		class?: string;
	}

	let { code, wrap = false, class: className = '' }: Props = $props();
</script>

<pre
	class="max-h-96 overflow-auto pre text-xs {wrap
		? 'break-all whitespace-pre-wrap'
		: ''} {className}">{code}</pre>
