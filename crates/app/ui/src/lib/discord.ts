// Discord bits shared by the guild and rule pages.

import { channels as channelsApi, isApiError } from './api';
import type { ChannelKind, GuildChannel, GuildMember, GuildRole, Rule } from './api/types';
import { settle } from './api/load';

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

/** Why a listed channel cannot be watched, for the ones that cannot. */
export function unwatchableReason(kind: ChannelKind): string {
	switch (kind) {
		case 'forum':
		case 'media':
			return 'Posts here are threads; watch a thread by its id';
		case 'category':
			return 'A category holds channels';
		default:
			return 'Not a message channel';
	}
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

/** How a channel is offered in a list: its name, and its kind when not a text channel. */
export function channelOptionLabel(channel: GuildChannel): string {
	const kind = channel.kind === 'text' ? '' : ` · ${CHANNEL_KIND_LABELS[channel.kind].toLowerCase()}`;
	return `${channelLabel(channel)}${kind}`;
}

export function channelById(channels: GuildChannel[] | null | undefined, id: string): GuildChannel | null {
	return channels?.find((c) => c.id === id) ?? null;
}

/** `#general` for a channel the bot lists, the id itself otherwise. */
export function channelName(channels: GuildChannel[] | null | undefined, id: string): string {
	const channel = channelById(channels, id);
	return channel ? channelLabel(channel) : id;
}

/** The key an application and a guild make, for grouping rules by guild. */
export function groupKey(applicationId: string, guildId: string): string {
	return `${applicationId}/${guildId}`;
}

/** The channels rules already watch, so no second rule is offered for them. */
export function takenChannels(rules: Rule[], except?: string | null): Set<string> {
	return new Set(rules.filter((rule) => rule.id !== except).map((rule) => rule.channel_id));
}

export function roleColor(role: GuildRole): string | null {
	return role.color ? `#${role.color.toString(16).padStart(6, '0')}` : null;
}

export function memberLabel(member: GuildMember): string {
	const name = member.nick ?? member.display_name ?? member.username;
	return name === member.username ? name : `${name} (${member.username})`;
}

/**
 * A guild's channels and roles as its bot lists them, for naming what a rule says.
 * Either list is `null` with the error beside it when the bot could not list it.
 */
export interface GuildDirectory {
	applicationId: string;
	guildId: string;
	channels: GuildChannel[] | null;
	channelsError: string | null;
	roles: GuildRole[] | null;
	rolesError: string | null;
}

/** Lists a guild's channels and roles; a list the bot cannot give leaves its error, not the page. */
export async function loadDirectory(applicationId: string, guildId: string): Promise<GuildDirectory> {
	const [channels, roles] = await Promise.all([
		settle(() => channelsApi.list(applicationId, guildId)),
		settle(() => channelsApi.roles(applicationId, guildId))
	]);
	return {
		applicationId,
		guildId,
		channels: channels.ok ? channels.value : null,
		channelsError: channels.ok ? null : channels.error,
		roles: roles.ok ? roles.value : null,
		rolesError: roles.ok ? null : roles.error
	};
}

/** Members of the guild whose name starts with `q`, through its bot. */
export function memberSearch(directory: GuildDirectory): (q: string) => Promise<GuildMember[]> {
	return (q) => channelsApi.members(directory.applicationId, directory.guildId, q);
}

/** One member of the guild by id through its bot; `null` when the id is not a member. */
export function memberLookup(directory: GuildDirectory): (userId: string) => Promise<GuildMember | null> {
	return async (userId) => {
		try {
			return await channelsApi.member(directory.applicationId, directory.guildId, userId);
		} catch (cause) {
			if (isApiError(cause) && cause.status === 404) return null;
			throw cause;
		}
	};
}
