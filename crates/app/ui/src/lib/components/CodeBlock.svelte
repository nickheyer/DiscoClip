<script lang="ts">
	interface Props {
		code: string;
		/** Break long tokens too, so nothing scrolls sideways. */
		wrap?: boolean;
		class?: string;
	}

	let { code, wrap = false, class: className = '' }: Props = $props();
</script>

<pre
	class="max-h-96 overflow-auto pre {wrap
		? 'break-all'
		: 'whitespace-pre'} {className}">{code}</pre>
