<script lang="ts">
	import ChannelSelect from './ChannelSelect.svelte';
	import Field from './Field.svelte';
	import MemberPicker from './MemberPicker.svelte';
	import RolePicker from './RolePicker.svelte';
	import TagInput from './TagInput.svelte';
	import type { RuleInput } from '$lib/api';
	import type { GuildDirectory } from '$lib/discord';
	import { memberLookup, memberSearch } from '$lib/discord';
	import { isSnowflake } from '$lib/format';

	interface Props {
		id: string;
		value: RuleInput;
		disabled?: boolean;
		/** The server the rule is for, so channels, roles and members are chosen by name. */
		directory?: GuildDirectory | null;
		/** Channels other rules of the server watch, not offered again. */
		taken?: Set<string>;
	}

	let { id, value = $bindable(), disabled = false, directory = null, taken = new Set() }: Props = $props();

	const snowflakeProblem = (v: string) => (isSnowflake(v) ? null : `${v} is not a Discord id`);
	// Use channel IDs for threads or unavailable directories.
	const channels = $derived(directory?.channels ?? null);
	let byId = $state(false);
	const pickChannels = $derived(channels !== null && !byId);
	const search = $derived(directory ? memberSearch(directory) : null);
	const lookup = $derived(directory ? memberLookup(directory) : null);

	let submitted = $state(false);
	const channelProblem = $derived(
		value.channel_id.trim() === ''
			? (submitted ? 'Choose a channel.' : null)
			: !isSnowflake(value.channel_id.trim())
				? 'Enter a valid Discord channel ID.'
				: taken.has(value.channel_id.trim())
					? 'Another rule already watches this channel'
					: null
	);
	const postToProblem = $derived(
		!value.post_to || value.post_to.trim() === '' || isSnowflake(value.post_to.trim())
			? null
			: 'Enter a valid Discord channel ID.'
	);

	export function valid(): boolean {
		submitted = true;
		return isSnowflake(value.channel_id.trim()) && !channelProblem && !postToProblem;
	}
</script>

<div class="stack">
	<fieldset class="form-section">
		<legend>Channels</legend>
	<div class="form-stack">
		<Field
			label="Watched channel"
			for={`${id}-channel`}
			hint={pickChannels
				? 'Download links posted in this channel.'
				: 'Enable Developer Mode in Discord, then right-click the channel and copy its ID.'}
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
			hint="Leave blank to post in the watched channel."
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
				<button type="button" class="linkish" onclick={() => (byId = true)} {disabled}>Enter channel IDs</button>
			{/if}
		</p>
	{:else if directory?.channelsError}
		<p class="error-text">The bot could not list the server's channels: {directory.channelsError}</p>
	{/if}

	</fieldset>
	<fieldset class="form-section">
		<legend>Who can submit links</legend>
		<p class="hint">Leave both lists empty to allow everyone.</p>
	<div class="form-stack">
		<Field
			label="Allowed users"
			for={`${id}-users`}
			optional
			hint="Allow links from these members."
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
			hint="Allow links from members with these roles."
			error={directory && !directory.roles ? `The bot could not list the server's roles: ${directory.rolesError}` : null}
		>
			{#if directory?.roles}
				<RolePicker id={`${id}-roles`} bind:values={value.allow_roles!} roles={directory.roles} guildId={directory.guildId} {disabled} />
			{:else}
				<TagInput id={`${id}-roles`} bind:values={value.allow_roles!} placeholder="Role id" validate={snowflakeProblem} {disabled} />
			{/if}
		</Field>
	</div>

	</fieldset>
	<label class="checkbox">
		<input type="checkbox" bind:checked={value.enabled} {disabled} />
		<span>
			<span class="strong">Enabled</span>
			<span class="hint">Download new links while this rule is enabled.</span>
		</span>
	</label>
</div>

<style>
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
