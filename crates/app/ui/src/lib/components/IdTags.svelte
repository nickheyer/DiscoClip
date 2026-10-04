<script lang="ts">
	import XIcon from '@lucide/svelte/icons/x';
	import { TagsInput } from '@skeletonlabs/skeleton-svelte';
	import type { Snowflake } from '$lib/api/types';

	interface Props {
		id?: string;
		/** The Discord ids listed */
		value: Snowflake[];
		/** What the box is for, for assistive technology */
		label: string;
		disabled?: boolean;
	}

	let { id, value = $bindable([]), label, disabled = false }: Props = $props();

	const SNOWFLAKE = /^\d{5,25}$/;
</script>

<!-- Skeleton's TagsInput, one id per tag, checked as it is typed -->
<TagsInput
	{value}
	onValueChange={(details) => (value = details.value)}
	validate={(details) =>
		SNOWFLAKE.test(details.inputValue) && !details.value.includes(details.inputValue)}
	{disabled}
>
	<TagsInput.Control>
		<TagsInput.Context>
			{#snippet children(tagsInput)}
				{#each tagsInput().value as tag, index (tag)}
					<TagsInput.Item value={tag} {index}>
						<TagsInput.ItemPreview class="font-mono">
							<TagsInput.ItemText>{tag}</TagsInput.ItemText>
							<TagsInput.ItemDeleteTrigger aria-label="Remove {tag}">
								<XIcon />
							</TagsInput.ItemDeleteTrigger>
						</TagsInput.ItemPreview>
						<TagsInput.ItemInput class="font-mono" />
					</TagsInput.Item>
				{/each}
			{/snippet}
		</TagsInput.Context>
		<TagsInput.Input {id} class="font-mono" aria-label={label} />
	</TagsInput.Control>
	<TagsInput.HiddenInput />
</TagsInput>
