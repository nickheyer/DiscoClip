<script lang="ts">
	import type { EffectivePolicy, Frontend } from '$lib/api/types';
	import BytesInput from '$lib/components/BytesInput.svelte';
	import Choice from '$lib/components/Choice.svelte';
	import Field from '$lib/components/Field.svelte';
	import HeightSelect from '$lib/components/HeightSelect.svelte';
	import NumberInput from '$lib/components/NumberInput.svelte';
	import { bytes, number } from '$lib/format';
	import {
		DELIVERY_MODES,
		OVER_LIMIT,
		UNDER_FLOOR,
		inherit,
		labelOf,
		type Option
	} from '$lib/policy';
	import type { DeliveryDraft, Problems, UploadDraft } from '$lib/profile';

	interface Props {
		upload: UploadDraft;
		value: DeliveryDraft;
		/** The content views a link may point at, as far as the account may list them */
		views: Frontend[];
		/** What the wider scopes give, shown where a choice is left to them */
		effective: EffectivePolicy;
		/** The built-in profile makes every choice, so nothing inherits */
		builtin: boolean;
		problems: Problems;
	}

	let {
		upload = $bindable(),
		value = $bindable(),
		views,
		effective,
		builtin,
		problems
	}: Props = $props();

	type UploadChoice = 'auto' | 'bytes';
	const UPLOADS: Option<UploadChoice>[] = [
		['auto', 'Auto'],
		['bytes', 'Fixed']
	];

	const inh = (shown: string) => (builtin ? undefined : inherit(shown));
	function hint<T>(shown: T): T | null {
		return builtin ? null : shown;
	}

	const viewName = (id: string) =>
		id === 'auto' ? 'Auto' : (views.find((v) => v.id === id)?.name ?? id);
	/** Every page a link may point at, plus the one chosen when the list lacks it */
	const pages = $derived<Option<string>[]>([
		['auto', 'Auto'],
		...views.map((view): Option<string> => [view.id, view.name]),
		...(value.view !== undefined && value.view !== 'auto' && !views.some((v) => v.id === value.view)
			? [[value.view, value.view] as Option<string>]
			: [])
	]);
	const inheritedUpload = $derived(
		effective.upload.max_bytes === 'auto' ? 'Auto' : bytes(effective.upload.max_bytes)
	);
</script>

<div class="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
	<Field label="Upload limit" for="upload-limit" error={problems['upload.max_bytes']}>
		<div class="grid gap-2">
			<Choice
				id="upload-limit"
				bind:value={
					() => (upload.limit === 'inherit' ? undefined : upload.limit),
					(chosen) => (upload.limit = chosen ?? 'inherit')
				}
				options={UPLOADS}
				inherit={inh(inheritedUpload)}
			/>
			{#if upload.limit === 'bytes'}
				<BytesInput bind:value={upload.maxBytes} />
			{/if}
		</div>
	</Field>
	<Field label="Delivery" for="delivery-mode" error={problems['delivery.mode']}>
		<Choice
			id="delivery-mode"
			bind:value={value.mode}
			options={DELIVERY_MODES}
			inherit={inh(labelOf(DELIVERY_MODES, effective.delivery.mode))}
		/>
	</Field>
	<Field label="Page" for="delivery-view" error={problems['delivery.view']}>
		<Choice
			id="delivery-view"
			bind:value={value.view}
			options={pages}
			inherit={inh(viewName(effective.delivery.view))}
		/>
	</Field>
	<Field
		label="Max page output"
		for="delivery-link-max"
		error={problems['delivery.link_max_bytes']}
	>
		<BytesInput
			id="delivery-link-max"
			bind:value={value.linkMaxBytes}
			placeholder={hint(effective.delivery.link_max_bytes)}
		/>
	</Field>
	<Field
		label="Floor height"
		for="delivery-floor-height"
		error={problems['delivery.floor.min_height']}
	>
		<HeightSelect
			id="delivery-floor-height"
			bind:value={value.minHeight}
			blank={inh(`${number(effective.delivery.floor.min_height)} px`)}
		/>
	</Field>
	<Field
		label="Floor bitrate"
		for="delivery-floor-bitrate"
		error={problems['delivery.floor.min_bitrate']}
	>
		<NumberInput
			id="delivery-floor-bitrate"
			bind:value={
				() => (value.minBitrate === null ? null : value.minBitrate / 1000),
				(kbps) => (value.minBitrate = kbps === null ? null : Math.round(kbps * 1000))
			}
			placeholder={hint(Math.round(effective.delivery.floor.min_bitrate / 1000))}
			min={1}
			step="any"
			unit="kbps"
		/>
	</Field>
	<Field label="Under the floor" for="delivery-under" error={problems['delivery.under_floor']}>
		<Choice
			id="delivery-under"
			bind:value={value.underFloor}
			options={UNDER_FLOOR}
			inherit={inh(labelOf(UNDER_FLOOR, effective.delivery.under_floor))}
		/>
	</Field>
	<Field label="Over the upload limit" for="delivery-over" error={problems['delivery.over_limit']}>
		<Choice
			id="delivery-over"
			bind:value={value.overLimit}
			options={OVER_LIMIT}
			inherit={inh(labelOf(OVER_LIMIT, effective.delivery.over_limit))}
		/>
	</Field>
</div>
