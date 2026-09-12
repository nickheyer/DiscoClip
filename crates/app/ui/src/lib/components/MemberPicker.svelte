<script lang="ts">
	import Avatar from './Avatar.svelte';
	import Badge from './Badge.svelte';
	import Icon from './Icon.svelte';
	import type { GuildMember } from '$lib/api';
	import { messageOf } from '$lib/api';
	import { memberLabel } from '$lib/discord';
	import { isSnowflake, userAvatarUrl } from '$lib/format';

	interface Props {
		id: string;
		values: string[];
		/** Members whose name starts with the text, through the guild's bot. */
		search: (q: string) => Promise<GuildMember[]>;
		/** One member by id; `null` when the id is not a member of the guild. */
		lookup: (userId: string) => Promise<GuildMember | null>;
		disabled?: boolean;
	}

	let { id, values = $bindable([]), search, lookup, disabled = false }: Props = $props();

	/** What the lookup said about an id: the member, none, or why it could not say. */
	type Known = { member: GuildMember | null } | { error: string };

	let known = $state<Record<string, Known>>({});
	const pending = new Set<string>();

	$effect(() => {
		for (const userId of values) {
			if (userId in known || pending.has(userId)) continue;
			pending.add(userId);
			lookup(userId)
				.then((member) => {
					known[userId] = { member };
				})
				.catch((cause: unknown) => {
					known[userId] = { error: messageOf(cause) };
				})
				.finally(() => pending.delete(userId));
		}
	});

	let input: HTMLInputElement | undefined = $state();
	let query = $state('');
	let results = $state<GuildMember[]>([]);
	let searching = $state(false);
	let searched = $state('');
	let searchError = $state<string | null>(null);
	let open = $state(false);
	let highlighted = $state(0);
	let timer: ReturnType<typeof setTimeout> | undefined;
	let latest = 0;

	function onInput() {
		searchError = null;
		clearTimeout(timer);
		const q = query.trim();
		if (!q || isSnowflake(q)) {
			results = [];
			searched = '';
			open = false;
			return;
		}
		timer = setTimeout(() => void run(q), 250);
	}

	async function run(q: string) {
		const ticket = ++latest;
		searching = true;
		try {
			const found = await search(q);
			if (ticket !== latest) return;
			for (const member of found) known[member.id] = { member };
			results = found.filter((member) => !values.includes(member.id));
			searched = q;
			highlighted = 0;
			open = true;
		} catch (cause) {
			if (ticket !== latest) return;
			searchError = messageOf(cause);
			results = [];
			searched = '';
			open = false;
		} finally {
			if (ticket === latest) searching = false;
		}
	}

	function add(userId: string) {
		if (!values.includes(userId)) values = [...values, userId];
		query = '';
		results = [];
		searched = '';
		open = false;
		input?.focus();
	}

	function remove(userId: string) {
		values = values.filter((v) => v !== userId);
		input?.focus();
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown' && results.length) {
			event.preventDefault();
			open = true;
			highlighted = (highlighted + 1) % results.length;
		} else if (event.key === 'ArrowUp' && results.length) {
			event.preventDefault();
			highlighted = (highlighted - 1 + results.length) % results.length;
		} else if (event.key === 'Enter') {
			event.preventDefault();
			const q = query.trim();
			if (open && results[highlighted]) add(results[highlighted].id);
			else if (isSnowflake(q)) add(q);
			else if (q) {
				clearTimeout(timer);
				void run(q);
			}
		} else if (event.key === 'Escape') {
			open = false;
		} else if (event.key === 'Backspace' && query === '' && values.length) {
			values = values.slice(0, -1);
		}
	}

	function onBlur() {
		// A click on a result lands before this closes the list.
		setTimeout(() => (open = false), 120);
	}

	function name(member: GuildMember): string {
		return member.nick ?? member.display_name ?? member.username;
	}
</script>

<div class={['picker', disabled && 'disabled']}>
	{#if values.length}
		<div class="chips">
			{#each values as userId (userId)}
				{@const info = known[userId]}
				{@const member = info && 'member' in info ? info.member : null}
				<span
					class={['tag', !member && 'unknown']}
					title={member
						? `${member.username} · ${userId}`
						: info && 'error' in info
							? `Could not look up ${userId}: ${info.error}`
							: info
								? `${userId} is not a member of this guild`
								: `Looking up ${userId}`}
				>
					{#if member}
						<Avatar name={member.username} size={18} src={userAvatarUrl(userId, member.avatar, 32)} />
						<span class="truncate">{memberLabel(member)}</span>
						{#if member.bot}<span class="bot">bot</span>{/if}
					{:else}
						<span class="truncate mono">{userId}</span>
						{#if info && 'member' in info}<span class="bot">not a member</span>{/if}
					{/if}
					{#if !disabled}
						<button type="button" class="remove" onclick={() => remove(userId)} aria-label={`Remove ${member ? member.username : userId}`}>
							<Icon name="x" size={12} />
						</button>
					{/if}
				</span>
			{/each}
		</div>
	{/if}
	<div class="search">
		<span class="glyph"><Icon name="search" size={14} /></span>
		<input
			{id}
			bind:this={input}
			bind:value={query}
			class="draft"
			role="combobox"
			aria-expanded={open}
			aria-controls={`${id}-results`}
			aria-autocomplete="list"
			aria-activedescendant={open && results[highlighted] ? `${id}-option-${results[highlighted].id}` : undefined}
			placeholder="Search members by name, or paste a user id"
			autocomplete="off"
			spellcheck="false"
			{disabled}
			oninput={onInput}
			onkeydown={onKeydown}
			onfocus={() => {
				if (results.length) open = true;
			}}
			onblur={onBlur}
		/>
		{#if searching}<span class="spinner" aria-hidden="true"></span>{/if}
	</div>
	<div class="anchor">
		{#if open && results.length}
			<ul class="results" role="listbox" id={`${id}-results`} aria-label="Members found">
				{#each results as member, i (member.id)}
					<li
						id={`${id}-option-${member.id}`}
						role="option"
						aria-selected={i === highlighted}
						class={['result', i === highlighted && 'active']}
						onmousedown={(event) => {
							event.preventDefault();
							add(member.id);
						}}
						onmouseenter={() => (highlighted = i)}
					>
						<Avatar name={member.username} size={24} src={userAvatarUrl(member.id, member.avatar, 32)} />
						<span class="who">
							<span class="strong truncate">{name(member)}</span>
							<span class="faint small truncate">@{member.username} · {member.id}</span>
						</span>
						{#if member.bot}<Badge size="sm">Bot</Badge>{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</div>
	{#if searchError}
		<p class="error-text" role="alert">Could not search members: {searchError}</p>
	{:else if open && searched && results.length === 0}
		<p class="hint">No member's name starts with “{searched}”. A user id can be pasted instead.</p>
	{/if}
</div>

<style>
	.picker {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}

	.tag {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		max-width: 100%;
		padding: 2px 4px 2px 4px;
		border-radius: 6px;
		background: var(--accent-soft);
		color: var(--accent-text);
		font-size: 12.5px;
		line-height: 1.5;
	}

	.tag.unknown {
		padding-left: 8px;
		background: var(--surface-3);
		color: var(--text-2);
	}

	.bot {
		padding: 0 5px;
		border-radius: 4px;
		background: color-mix(in srgb, currentColor 12%, transparent);
		font-size: 10.5px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
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

	.search {
		display: flex;
		align-items: center;
		gap: 6px;
		min-height: 36px;
		padding: 0 10px;
		border: 1px solid var(--border-strong);
		border-radius: var(--radius-sm);
		background: var(--surface);
	}

	.search:focus-within {
		border-color: var(--accent);
		box-shadow: var(--focus);
	}

	.disabled .search {
		background: var(--surface-2);
	}

	.glyph {
		display: flex;
		color: var(--text-3);
	}

	.draft {
		flex: 1;
		min-width: 0;
		border: none;
		background: transparent;
		padding: 6px 0;
		outline: none;
	}

	.draft::placeholder {
		color: var(--text-3);
	}

	.spinner {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		border: 2px solid var(--border-strong);
		border-top-color: var(--accent);
		animation: spin 0.8s linear infinite;
		flex: none;
	}

	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	.anchor {
		position: relative;
		height: 0;
	}

	.results {
		position: absolute;
		top: -4px;
		left: 0;
		right: 0;
		z-index: 30;
		margin: 0;
		padding: 4px;
		list-style: none;
		max-height: 260px;
		overflow-y: auto;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--radius-sm);
		box-shadow: var(--shadow-lg);
	}

	.result {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 6px 8px;
		border-radius: var(--radius-xs);
		cursor: pointer;
	}

	.result.active {
		background: var(--accent-soft);
	}

	.who {
		display: flex;
		flex-direction: column;
		min-width: 0;
		flex: 1;
		line-height: 1.25;
	}
</style>
