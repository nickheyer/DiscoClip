// The words for every policy choice, in the order a select offers them

import type {
	BotMessages,
	DedupeMatch,
	DeliveryMode,
	OriginalEmbeds,
	OriginalText,
	OverLimit,
	PermissionMode,
	Placement,
	ReplaceAs,
	Requester,
	UnderFloor
} from './api/types';

/** A value and the word for it */
export type Option<V> = readonly [V, string];

export const ON_OFF: Option<boolean>[] = [
	[true, 'On'],
	[false, 'Off']
];
export const BOT_MESSAGES: Option<BotMessages>[] = [
	['ignore', 'Ignored'],
	['accept', 'Accepted']
];
export const PLACEMENTS: Option<Placement>[] = [
	['reply', 'Reply'],
	['post', 'Post'],
	['replace', 'Replace the message']
];
export const REPLACE_AS: Option<ReplaceAs>[] = [
	['author', 'The author'],
	['bot', 'The bot']
];
export const ORIGINAL_TEXT: Option<OriginalText>[] = [
	['keep', 'Kept'],
	['drop', 'Dropped']
];
export const ORIGINAL_EMBEDS: Option<OriginalEmbeds>[] = [
	['keep', 'Kept'],
	['suppress', 'Suppressed']
];
export const PERMISSION_MODES: Option<PermissionMode>[] = [
	['check', 'Checked first'],
	['assume', 'Assumed']
];
export const REQUESTERS: Option<Requester>[] = [
	['none', 'Left out'],
	['name', 'Name'],
	['mention', 'Mention']
];
export const DELIVERY_MODES: Option<DeliveryMode>[] = [
	['auto', 'Auto'],
	['upload', 'Upload'],
	['link', 'Link']
];
export const UNDER_FLOOR: Option<UnderFloor>[] = [
	['skip', 'Skip'],
	['link', 'Link'],
	['upload', 'Upload anyway']
];
export const OVER_LIMIT: Option<OverLimit>[] = [
	['skip', 'Skip'],
	['link', 'Link']
];
export const DEDUPE_MATCHES: Option<DedupeMatch>[] = [
	['either', 'Link or content'],
	['url', 'Link'],
	['content', 'Content']
];

/** The word for a value among the options, or the value spelled as it is */
export function labelOf<V>(options: readonly Option<V>[], value: V): string {
	return options.find(([v]) => v === value)?.[1] ?? String(value);
}

/** The inherit entry's wording, naming what the wider scope gives */
export function inherit(shown: string): string {
	return `Inherit (${shown})`;
}
