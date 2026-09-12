<script lang="ts">
	import Icon from './Icon.svelte';
	import type { GuildRole } from '$lib/api';
	import { roleColor } from '$lib/discord';

	interface Props {
		id: string;
		values: string[];
		/** The guild's roles as the bot lists them, highest first. */
		roles: GuildRole[];
		/** The guild's id, which is also its @everyone role: never offered. */
		guildId: string;
		disabled?: boolean;
	}

	let { id, values = $bindable([]), roles, guildId, disabled = false }: Props = $props();

	const byId = $derived(new Map(roles.map((role) => [role.id, role])));
	const offered = $derived(roles.filter((role) => role.id !== guildId && !values.includes(role.id)));

	function onChange(event: Event) {
		const select = event.currentTarget as HTMLSelectElement;
		const roleId = select.value;
		if (roleId && !values.includes(roleId)) values = [...values, roleId];
		select.value = '';
	}

	function remove(roleId: string) {
		values = values.filter((v) => v !== roleId);
	}
</script>

<div class="picker">
	{#if values.length}
		<div class="chips">
			{#each values as roleId (roleId)}
				{@const role = byId.get(roleId)}
				{@const color = role ? roleColor(role) : null}
				<span
					class={['tag', !role && 'unknown']}
					style={color ? `--role: ${color}` : undefined}
					title={role ? `Role ${roleId}` : `Role ${roleId} is not a role of this guild any more`}
				>
					<span class="dot" aria-hidden="true"></span>
					<span class="truncate">{role ? role.name : `Role ${roleId}`}</span>
					{#if !disabled}
						<button type="button" class="remove" onclick={() => remove(roleId)} aria-label={`Remove ${role?.name ?? roleId}`}>
							<Icon name="x" size={12} />
						</button>
					{/if}
				</span>
			{/each}
		</div>
	{/if}
	<select {id} class="select" value="" onchange={onChange} disabled={disabled || offered.length === 0}>
		<option value="">
			{offered.length ? 'Add a role…' : values.length ? 'Every role is chosen' : 'The guild has no roles to choose'}
		</option>
		{#each offered as role (role.id)}
			<option value={role.id}>{role.name}{role.managed ? ' (integration)' : ''}</option>
		{/each}
	</select>
</div>

<style>
	.picker {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}

	.tag {
		--role: var(--text-3);
		display: inline-flex;
		align-items: center;
		gap: 6px;
		max-width: 100%;
		padding: 2px 4px 2px 8px;
		border-radius: 6px;
		background: var(--surface-3);
		color: var(--text);
		font-size: 12.5px;
		line-height: 1.5;
	}

	.tag.unknown {
		color: var(--text-3);
		font-family: var(--font-mono);
	}

	.dot {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		background: var(--role);
		flex: none;
	}

	.remove {
		display: flex;
		padding: 2px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: inherit;
		cursor: pointer;
		opacity: 0.7;
	}

	.remove:hover {
		opacity: 1;
		background: color-mix(in srgb, currentColor 15%, transparent);
	}
</style>
