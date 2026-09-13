// What the server's live feed carries, as the shared worker and the tabs pass it around.

export type FeedState = 'idle' | 'connecting' | 'live' | 'reconnecting';

/** The server-sent events on the feed, by name. */
export const LIVE_EVENT_NAMES = ['stats', 'job', 'bot'] as const;
export type LiveEventName = (typeof LIVE_EVENT_NAMES)[number];

/** What the worker tells a tab: the connection's state, or an event as the server sent it. */
export type LiveMessage =
	| { type: 'state'; state: FeedState }
	| { type: 'event'; name: LiveEventName; data: string };

/** What a tab tells the worker. */
export type LiveCommand = 'start' | 'stop';
