// Discord bits shared by the guild and rule pages.

import type { ChannelKind, GuildChannel, GuildMember, GuildRole } from './api/types';

export const CHANNEL_KIND_LABELS: Record<ChannelKind, string> = {
	text: 'Text',
	announcement: 'Announcements',
	voice: 'Voice',
	stage: 'Stage',
	category: 'Category',
	forum: 'Forum',
	media: 'Media',
	thread: 'Thread',
	other: 'Other'
};

/** Channels a bot can watch for links: the ones messages are posted in. */
export function watchable(kind: ChannelKind): boolean {
	return kind === 'text' || kind === 'announcement' || kind === 'voice' || kind === 'stage' || kind === 'thread';
}

/** The glyph Discord shows before a channel's name. */
export function channelGlyph(kind: ChannelKind): string {
	switch (kind) {
		case 'voice':
		case 'stage':
			return 'volume';
		case 'announcement':
			return 'megaphone';
		case 'forum':
		case 'media':
			return 'messages';
		case 'thread':
			return 'thread';
		case 'category':
			return 'folder';
		default:
			return 'hash';
	}
}

export interface ChannelGroup {
	category: GuildChannel | null;
	channels: GuildChannel[];
}

/** Channels under their categories in Discord's order, the uncategorised ones first. */
export function groupChannels(channels: GuildChannel[]): ChannelGroup[] {
	const byPosition = (a: GuildChannel, b: GuildChannel) => a.position - b.position || a.name.localeCompare(b.name);
	const categories = channels.filter((c) => c.kind === 'category').sort(byPosition);
	const groups: ChannelGroup[] = [];
	const loose = channels.filter((c) => c.kind !== 'category' && !c.parent_id).sort(byPosition);
	if (loose.length) groups.push({ category: null, channels: loose });
	for (const category of categories) {
		groups.push({
			category,
			channels: channels.filter((c) => c.parent_id === category.id).sort(byPosition)
		});
	}
	const known = new Set(categories.map((c) => c.id));
	const orphans = channels
		.filter((c) => c.kind !== 'category' && c.parent_id && !known.has(c.parent_id))
		.sort(byPosition);
	if (orphans.length) groups.push({ category: null, channels: orphans });
	return groups;
}

export function channelLabel(channel: GuildChannel): string {
	return `#${channel.name}`;
}

export function roleColor(role: GuildRole): string | null {
	return role.color ? `#${role.color.toString(16).padStart(6, '0')}` : null;
}

export function memberLabel(member: GuildMember): string {
	const name = member.nick ?? member.display_name ?? member.username;
	return name === member.username ? name : `${name} (${member.username})`;
}
