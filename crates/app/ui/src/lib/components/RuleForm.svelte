<script lang="ts">
	import ChannelSelect from './ChannelSelect.svelte';
	import Field from './Field.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import RolePicker from './RolePicker.svelte';
	import TagInput from './TagInput.svelte';
	import type { RuleInput } from '$lib/api';
	import type { GuildDirectory } from '$lib/discord';
	import { memberLookup, memberSearch } from '$lib/discord';
	import { formatBytes, formatDuration, isSnowflake } from '$lib/format';

	interface Props {
		id: string;
		value: RuleInput;
		disabled?: boolean;
		/** The guild the rule is for, so channels, roles and members are chosen by name. */
		directory?: GuildDirectory | null;
		/** Channels other rules of the guild watch, not offered again. */
		taken?: Set<string>;
	}

	let { id, value = $bindable(), disabled = false, directory = null, taken = new Set() }: Props = $props();

	const snowflakeProblem = (v: string) => (isSnowflake(v) ? null : `${v} is not a Discord id`);
	const hostProblem = (v: string) =>
		/^(?=.{1,253}$)(\*\.)?([a-z0-9-]+\.)+[a-z0-9-]+$/i.test(v) ? null : `${v} is not a host name`;

	const BYTE_UNITS = [
		{ label: 'MB', factor: 1024 * 1024 },
		{ label: 'GB', factor: 1024 * 1024 * 1024 }
	];

	// Channels are picked from the bot's list, or entered by id: for a thread, which the
	// list leaves out, or when the list could not be fetched.
	const channels = $derived(directory?.channels ?? null);
	let byId = $state(false);
	const pickChannels = $derived(channels !== null && !byId);
	const search = $derived(directory ? memberSearch(directory) : null);
	const lookup = $derived(directory ? memberLookup(directory) : null);

	// Size and duration are entered in human units and stored as the API wants them.
	let sizeUnit = $state<'MB' | 'GB'>(value.max_source_bytes && value.max_source_bytes >= 1024 ** 3 ? 'GB' : 'MB');
	let sizeText = $state(
		value.max_source_bytes == null
			? ''
			: String(
					+(
						value.max_source_bytes /
						(value.max_source_bytes >= 1024 ** 3 ? 1024 ** 3 : 1024 ** 2)
					).toFixed(2)
				)
	);
	let durationText = $state(value.max_duration_secs == null ? '' : String(Math.round(value.max_duration_secs / 60)));
	let heightText = $state(value.max_height == null ? '' : String(value.max_height));

	const sizeProblem = $derived(sizeText !== '' && !(Number(sizeText) > 0) ? 'A size is a positive number' : null);
	const durationProblem = $derived(
		durationText !== '' && !(Number(durationText) > 0) ? 'A duration is a positive number of minutes' : null
	);
	const heightProblem = $derived(
		heightText !== '' && !(Number.isInteger(Number(heightText)) && Number(heightText) > 0)
			? 'A height is a positive whole number of pixels'
			: null
	);
	const channelProblem = $derived(
		value.channel_id.trim() === ''
			? null
			: !isSnowflake(value.channel_id.trim())
				? 'A channel id is a Discord snowflake'
				: taken.has(value.channel_id.trim())
					? 'Another rule already watches this channel'
					: null
	);
	const postToProblem = $derived(
		!value.post_to || value.post_to.trim() === '' || isSnowflake(value.post_to.trim())
			? null
			: 'A channel id is a Discord snowflake'
	);

	$effect(() => {
		const factor = BYTE_UNITS.find((u) => u.label === sizeUnit)!.factor;
		value.max_source_bytes = sizeText === '' || sizeProblem ? null : Math.round(Number(sizeText) * factor);
	});
	$effect(() => {
		value.max_duration_secs =
			durationText === '' || durationProblem ? null : Math.round(Number(durationText) * 60);
	});
	$effect(() => {
		value.max_height = heightText === '' || heightProblem ? null : Number(heightText);
	});

	export function valid(): boolean {
		return (
			isSnowflake(value.channel_id.trim()) &&
			!channelProblem &&
			!postToProblem &&
			!sizeProblem &&
			!durationProblem &&
			!heightProblem
		);
	}
</script>

<div class="stack">
	<div class="grid-2">
		<Field
			label="Watched channel"
			for={`${id}-channel`}
			hint={pickChannels
				? 'The channel whose links are picked up.'
				: 'The id of the channel whose links are picked up. Right-click a channel in Discord with developer mode on and copy its id.'}
			error={channelProblem}
		>
			{#if pickChannels && channels}
				<ChannelSelect
					id={`${id}-channel`}
					bind:value={value.channel_id}
					{channels}
					{taken}
					{disabled}
					invalid={!!channelProblem}
				/>
			{:else}
				<input
					id={`${id}-channel`}
					class="input mono"
					bind:value={value.channel_id}
					placeholder="123456789012345678"
					inputmode="numeric"
					autocomplete="off"
					spellcheck="false"
					required
					{disabled}
					aria-invalid={channelProblem ? 'true' : undefined}
				/>
			{/if}
		</Field>
		<Field
			label="Post results to"
			for={`${id}-post`}
			optional
			hint="Where finished clips are posted. Leave empty to post in the watched channel."
			error={postToProblem}
		>
			{#if pickChannels && channels}
				<ChannelSelect
					id={`${id}-post`}
					value={value.post_to ?? ''}
					onchange={(chosen) => (value.post_to = chosen || null)}
					{channels}
					emptyLabel="Same channel"
					{disabled}
					invalid={!!postToProblem}
				/>
			{:else}
				<input
					id={`${id}-post`}
					class="input mono"
					value={value.post_to ?? ''}
					oninput={(e) => (value.post_to = (e.currentTarget as HTMLInputElement).value.trim() || null)}
					placeholder="Same channel"
					inputmode="numeric"
					autocomplete="off"
					spellcheck="false"
					{disabled}
					aria-invalid={postToProblem ? 'true' : undefined}
				/>
			{/if}
		</Field>
	</div>
	{#if channels}
		<p class="hint switch">
			{#if byId}
				<button type="button" class="linkish" onclick={() => (byId = false)} {disabled}>Choose channels from the list</button>
			{:else}
				Threads are not in the list.
				<button type="button" class="linkish" onclick={() => (byId = true)} {disabled}>Enter channel ids instead</button>
			{/if}
		</p>
	{:else if directory?.channelsError}
		<p class="error-text">The bot could not list the guild's channels: {directory.channelsError}</p>
	{/if}

	<Field
		label="Allowed hosts"
		for={`${id}-hosts`}
		optional
		hint="Only links to these hosts count, such as youtube.com or *.tiktok.com. Empty means every supported host."
	>
		<TagInput
			id={`${id}-hosts`}
			bind:values={value.allow_hosts!}
			placeholder="youtube.com"
			validate={hostProblem}
			normalize={(v) => v.trim().toLowerCase().replace(/^https?:\/\//, '').replace(/\/.*$/, '')}
			{disabled}
		/>
	</Field>

	<div class="grid-2">
		<Field
			label="Allowed users"
			for={`${id}-users`}
			optional
			hint="Members whose links count. Empty, with no roles, means everyone."
		>
			{#if search && lookup}
				<MemberPicker id={`${id}-users`} bind:values={value.allow_users!} {search} {lookup} {disabled} />
			{:else}
				<TagInput id={`${id}-users`} bind:values={value.allow_users!} placeholder="User id" validate={snowflakeProblem} {disabled} />
			{/if}
		</Field>
		<Field
			label="Allowed roles"
			for={`${id}-roles`}
			optional
			hint="Roles whose members' links count."
			error={directory && !directory.roles ? `The bot could not list the guild's roles: ${directory.rolesError}` : null}
		>
			{#if directory?.roles}
				<RolePicker id={`${id}-roles`} bind:values={value.allow_roles!} roles={directory.roles} guildId={directory.guildId} {disabled} />
			{:else}
				<TagInput id={`${id}-roles`} bind:values={value.allow_roles!} placeholder="Role id" validate={snowflakeProblem} {disabled} />
			{/if}
		</Field>
	</div>

	<div class="grid-3">
		<Field
			label="Largest source"
			for={`${id}-size`}
			optional
			hint={value.max_source_bytes ? `Downloads over ${formatBytes(value.max_source_bytes)} are refused.` : 'No limit beyond the server’s own.'}
			error={sizeProblem}
		>
			<div class="unit">
				<input
					id={`${id}-size`}
					class="input"
					type="number"
					min="0"
					step="any"
					bind:value={sizeText}
					placeholder="No limit"
					{disabled}
					aria-invalid={sizeProblem ? 'true' : undefined}
				/>
				<select class="select" bind:value={sizeUnit} aria-label="Size unit" {disabled}>
					{#each BYTE_UNITS as unit (unit.label)}
						<option value={unit.label}>{unit.label}</option>
					{/each}
				</select>
			</div>
		</Field>
		<Field
			label="Longest video"
			for={`${id}-duration`}
			optional
			hint={value.max_duration_secs ? `Videos over ${formatDuration(value.max_duration_secs)} are refused.` : 'In minutes. No limit beyond the server’s own.'}
			error={durationProblem}
		>
			<div class="unit">
				<input
					id={`${id}-duration`}
					class="input"
					type="number"
					min="0"
					step="any"
					bind:value={durationText}
					placeholder="No limit"
					{disabled}
					aria-invalid={durationProblem ? 'true' : undefined}
				/>
				<span class="unit-label">min</span>
			</div>
		</Field>
		<Field
			label="Tallest output"
			for={`${id}-height`}
			optional
			hint="In pixels; taller sources are downscaled. Tightens the server’s own limit."
			error={heightProblem}
		>
			<div class="unit">
				<input
					id={`${id}-height`}
					class="input"
					type="number"
					min="0"
					step="1"
					bind:value={heightText}
					placeholder="Server limit"
					{disabled}
					aria-invalid={heightProblem ? 'true' : undefined}
				/>
				<span class="unit-label">px</span>
			</div>
		</Field>
	</div>

	<label class="checkbox">
		<input type="checkbox" bind:checked={value.enabled} {disabled} />
		<span>
			<span class="strong">Enabled</span>
			<span class="hint">A disabled rule is kept but its channel is not watched.</span>
		</span>
	</label>
</div>

<style>
	.unit {
		display: grid;
		grid-template-columns: 1fr auto;
		gap: 6px;
		align-items: center;
	}

	.unit .select {
		width: 84px;
	}

	.unit-label {
		color: var(--text-3);
		font-size: 12.5px;
		padding: 0 6px;
	}

	.switch {
		margin-top: -8px;
	}

	.linkish {
		padding: 0;
		border: none;
		background: none;
		color: var(--accent-text);
		font-size: inherit;
		cursor: pointer;
	}

	.linkish:hover:not(:disabled) {
		text-decoration: underline;
	}

	.linkish:disabled {
		cursor: not-allowed;
		opacity: 0.6;
	}
</style>
