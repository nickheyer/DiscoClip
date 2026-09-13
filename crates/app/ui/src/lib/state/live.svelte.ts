import LiveWorker from '$lib/live.worker?sharedworker';
import { LIVE_EVENTS_URL } from '$lib/api/endpoints';
import { LIVE_EVENT_NAMES } from '$lib/live';
import type { FeedState, LiveEventName, LiveMessage } from '$lib/live';

type Handler = (name: LiveEventName, data: string) => void;

/** A connection to the feed: through the browser's shared worker, or from this tab. */
interface Link {
	close(): void;
}

/**
 * The server's live feed: the job stats, job events and bot statuses, on one connection
 * for the whole browser, held by a shared worker every tab connects to. A browser without
 * shared workers opens the stream from the tab. The feed's users, the job and bot stores,
 * start and stop it; the connection opens with the first and closes with the last.
 */
class Live {
	state = $state<FeedState>('idle');
	private handlers = new Set<Handler>();
	private link: Link | null = null;
	private users = 0;

	/** Runs `handler` for every event until the returned function is called. */
	on(handler: Handler): () => void {
		this.handlers.add(handler);
		return () => this.handlers.delete(handler);
	}

	start(): void {
		this.users += 1;
		if (this.link) return;
		this.state = 'connecting';
		this.link = typeof SharedWorker === 'undefined' ? this.direct() : this.shared();
	}

	stop(): void {
		if (this.users > 0) this.users -= 1;
		if (this.users > 0 || !this.link) return;
		this.link.close();
		this.link = null;
		this.state = 'idle';
	}

	private take(message: LiveMessage): void {
		if (message.type === 'state') {
			this.state = message.state;
			return;
		}
		for (const handler of this.handlers) handler(message.name, message.data);
	}

	private shared(): Link {
		const worker = new LiveWorker({ name: 'discoclip-live' });
		worker.port.onmessage = (event: MessageEvent<LiveMessage>) => this.take(event.data);
		worker.port.postMessage('start');
		return {
			close: () => {
				worker.port.postMessage('stop');
				worker.port.close();
			}
		};
	}

	private direct(): Link {
		const source = new EventSource(LIVE_EVENTS_URL, { withCredentials: true });
		source.onopen = () => {
			this.state = 'live';
		};
		source.onerror = () => {
			this.state = source.readyState === EventSource.CLOSED ? 'idle' : 'reconnecting';
		};
		for (const name of LIVE_EVENT_NAMES) {
			source.addEventListener(name, (event) =>
				this.take({ type: 'event', name, data: (event as MessageEvent<string>).data })
			);
		}
		return { close: () => source.close() };
	}
}

export const live = new Live();
