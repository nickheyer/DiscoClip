// Shaping a guild's channel list the way people think of it: categories, then channels,
// then threads under their channel.

import type { ChannelKind, GuildChannel, Snowflake } from '$lib/api/types';

export interface ChannelNode {
	id: string;
	name: string;
	kind: ChannelKind;
	channel: GuildChannel | null;
	children?: ChannelNode[];
}

const KIND_ORDER: ChannelKind[] = [
	'text',
	'announcement',
	'forum',
	'media',
	'voice',
	'stage',
	'thread',
	'other',
	'category'
];

const byPosition = (a: GuildChannel, b: GuildChannel) =>
	a.position - b.position ||
	KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) ||
	a.name.localeCompare(b.name);

/** The channels as a tree: uncategorised channels first, then each category with its channels. */
export function channelTree(channels: GuildChannel[]): ChannelNode[] {
	const byId = new Map(channels.map((c) => [c.id, c]));
	const threadsOf = new Map<Snowflake, GuildChannel[]>();
	const childrenOf = new Map<Snowflake | null, GuildChannel[]>();
	for (const channel of channels) {
		if (channel.kind === 'category') continue;
		const parent = channel.parent_id ? byId.get(channel.parent_id) : undefined;
		if (channel.kind === 'thread' && parent && parent.kind !== 'category') {
			threadsOf.set(parent.id, [...(threadsOf.get(parent.id) ?? []), channel]);
			continue;
		}
		const key = parent && parent.kind === 'category' ? parent.id : null;
		childrenOf.set(key, [...(childrenOf.get(key) ?? []), channel]);
	}
	const leaf = (channel: GuildChannel): ChannelNode => {
		const threads = (threadsOf.get(channel.id) ?? []).sort(byPosition);
		return {
			id: channel.id,
			name: channel.name,
			kind: channel.kind,
			channel,
			children: threads.length > 0 ? threads.map(leaf) : undefined
		};
	};
	const top = (childrenOf.get(null) ?? []).sort(byPosition).map(leaf);
	const categories = channels
		.filter((c) => c.kind === 'category')
		.sort(byPosition)
		.map((category): ChannelNode => ({
			id: category.id,
			name: category.name,
			kind: 'category',
			channel: category,
			children: (childrenOf.get(category.id) ?? []).sort(byPosition).map(leaf)
		}));
	return [...top, ...categories];
}

/** Channels a rule can watch, in tree order, with their category for the label. */
export function watchable(
	channels: GuildChannel[]
): { channel: GuildChannel; category: string | null }[] {
	const out: { channel: GuildChannel; category: string | null }[] = [];
	const walk = (nodes: ChannelNode[], category: string | null) => {
		for (const node of nodes) {
			if (node.kind === 'category') {
				walk(node.children ?? [], node.name);
				continue;
			}
			if (node.channel) out.push({ channel: node.channel, category });
			walk(node.children ?? [], category);
		}
	};
	walk(channelTree(channels), null);
	return out;
}

/** `#name` for text-like channels, the plain name otherwise. */
export function channelLabel(channel: GuildChannel | undefined, id?: Snowflake): string {
	if (!channel) return id ?? '';
	switch (channel.kind) {
		case 'text':
		case 'announcement':
		case 'forum':
		case 'media':
			return `#${channel.name}`;
		default:
			return channel.name;
	}
}
