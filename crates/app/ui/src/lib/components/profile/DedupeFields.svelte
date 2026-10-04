<script lang="ts">
	import type { EffectivePolicy } from '$lib/api/types';
	import Choice from '$lib/components/Choice.svelte';
	import Field from '$lib/components/Field.svelte';
	import { DEDUPE_MATCHES, ON_OFF, inherit, labelOf } from '$lib/policy';
	import type { DedupeDraft, Problems } from '$lib/profile';

	interface Props {
		value: DedupeDraft;
		/** What the wider scopes give, shown where a choice is left to them */
		effective: EffectivePolicy;
		/** The built-in profile makes every choice, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let { value = $bindable(), effective, builtin, problems }: Props = $props();

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Repost from the archive" for="dedupe-enabled" error={problems['dedupe.enabled']}>
		<Choice
			id="dedupe-enabled"
			bind:value={value.enabled}
			options={ON_OFF}
			inherit={inh(labelOf(ON_OFF, effective.dedupe.enabled))}
		/>
	</Field>
	<Field label="Matched by" for="dedupe-match" error={problems['dedupe.match']}>
		<Choice
			id="dedupe-match"
			bind:value={value.match}
			options={DEDUPE_MATCHES}
			inherit={inh(labelOf(DEDUPE_MATCHES, effective.dedupe.match))}
		/>
	</Field>
</div>
