// The live feed: one `EventSource` on `/api/events` per browser.
//
// Every tab asks for the `discoclip-events` Web Lock. The tab that holds it connects and
// rebroadcasts each event over a `BroadcastChannel` of the same name; the others consume from
// the channel and take over the moment the lock frees. Where the page runs without the Web
// Locks API, on plain HTTP away from localhost, each tab holds a connection of its own.

import type { BotEvent, BotStatus, JobEvent, JobStats, Uuid } from './api/types';
import { streams } from './api/endpoints';

export type FeedState = 'connecting' | 'live' | 'offline';

const NAME = 'discoclip-events';
const MIN_BACKOFF = 1_000;
const MAX_BACKOFF = 30_000;

type FeedEventName = 'stats' | 'job' | 'bot';

type Message =
	| { type: 'hello' }
	| { type: 'snapshot'; state: FeedState; stats: JobStats | null; bots: Record<Uuid, BotStatus> }
	| { type: 'state'; state: FeedState }
	| { type: 'event'; name: FeedEventName; data: unknown };

type JobHandler = (event: JobEvent) => void;

class Feed {
	state = $state<FeedState>('connecting');
	/** The latest job statistics. */
	stats = $state<JobStats | null>(null);
	/** Every bot's latest status by application id. */
	bots = $state<Record<Uuid, BotStatus>>({});

	#handlers = new Set<JobHandler>();
	#channel: BroadcastChannel | null = null;
	#source: EventSource | null = null;
	#timer: ReturnType<typeof setTimeout> | null = null;
	#backoff = MIN_BACKOFF;
	#started = false;
	#leading = false;
	#abort: AbortController | null = null;
	#release: (() => void) | null = null;

	/** Runs `handler` for every job event. Returns the way to stop. */
	onJob(handler: JobHandler): () => void {
		this.#handlers.add(handler);
		return () => {
			this.#handlers.delete(handler);
		};
	}

	/** Joins the feed. Safe to call more than once. */
	start(): void {
		if (this.#started) return;
		this.#started = true;
		if ('BroadcastChannel' in globalThis) {
			this.#channel = new BroadcastChannel(NAME);
			this.#channel.onmessage = (event: MessageEvent<Message>) => this.#receive(event.data);
			this.#post({ type: 'hello' });
		}
		if (typeof navigator !== 'undefined' && navigator.locks) {
			this.#abort = new AbortController();
			navigator.locks
				.request(
					NAME,
					{ signal: this.#abort.signal },
					() =>
						new Promise<void>((resolve) => {
							this.#release = resolve;
							this.#lead();
						})
				)
				.catch(() => {
					// The request was aborted by `stop()`.
				});
		} else {
			this.#lead();
		}
	}

	/** Leaves the feed and frees the lock for another tab. */
	stop(): void {
		if (!this.#started) return;
		this.#started = false;
		this.#abort?.abort();
		this.#abort = null;
		this.#disconnect();
		this.#leading = false;
		this.#release?.();
		this.#release = null;
		this.#channel?.close();
		this.#channel = null;
		this.state = 'connecting';
		this.stats = null;
		this.bots = {};
	}

	#post(message: Message): void {
		this.#channel?.postMessage(message);
	}

	#lead(): void {
		if (!this.#started) return;
		this.#leading = true;
		this.#connect();
	}

	#setState(state: FeedState): void {
		this.state = state;
		if (this.#leading) this.#post({ type: 'state', state });
	}

	#connect(): void {
		if (!this.#started || !this.#leading) return;
		this.#setState('connecting');
		const source = new EventSource(streams.events());
		this.#source = source;
		source.onopen = () => {
			this.#backoff = MIN_BACKOFF;
			this.#setState('live');
		};
		for (const name of ['stats', 'job', 'bot'] as const) {
			source.addEventListener(name, (event: MessageEvent<string>) => {
				const data: unknown = JSON.parse(event.data);
				this.#apply(name, data);
				this.#post({ type: 'event', name, data });
			});
		}
		source.onerror = () => {
			if (this.#source !== source) return;
			source.close();
			this.#source = null;
			this.#setState('offline');
			this.#timer = setTimeout(() => {
				this.#timer = null;
				this.#connect();
			}, this.#backoff);
			this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF);
		};
	}

	#disconnect(): void {
		if (this.#timer !== null) {
			clearTimeout(this.#timer);
			this.#timer = null;
		}
		this.#source?.close();
		this.#source = null;
		this.#backoff = MIN_BACKOFF;
	}

	#apply(name: FeedEventName, data: unknown): void {
		switch (name) {
			case 'stats':
				this.stats = data as JobStats;
				break;
			case 'bot': {
				const event = data as BotEvent;
				if (event.removed) {
					const rest = { ...this.bots };
					delete rest[event.application];
					this.bots = rest;
				} else {
					const status: Partial<BotEvent> = { ...event };
					delete status.application;
					delete status.removed;
					this.bots = { ...this.bots, [event.application]: status as BotStatus };
				}
				break;
			}
			case 'job':
				for (const handler of this.#handlers) handler(data as JobEvent);
				break;
		}
	}

	#receive(message: Message): void {
		if (this.#leading) {
			if (message.type === 'hello') {
				this.#post({
					type: 'snapshot',
					state: this.state,
					stats: $state.snapshot(this.stats),
					bots: $state.snapshot(this.bots)
				});
			}
			return;
		}
		switch (message.type) {
			case 'event':
				this.#apply(message.name, message.data);
				break;
			case 'state':
				this.state = message.state;
				break;
			case 'snapshot':
				this.state = message.state;
				this.stats = message.stats;
				this.bots = message.bots;
				break;
			case 'hello':
				break;
		}
	}
}

export const feed = new Feed();
