<script lang="ts">
	import { number } from '$lib/format';

	interface Props {
		value: number;
		/** The thing counted, singular. */
		noun: string;
		/** Its plural, when it is not the singular with an s. */
		plural?: string;
		class?: string;
	}

	let { value, noun, plural, class: className = '' }: Props = $props();
</script>

<span class="tabular-nums {className}"
	>{number(value)} {value === 1 ? noun : (plural ?? `${noun}s`)}</span
>
