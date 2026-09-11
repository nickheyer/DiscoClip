import { applications } from '$lib/api';
import type { BotEvent, BotStatus } from '$lib/api';

export type FeedState = 'idle' | 'connecting' | 'live' | 'reconnecting';

/** Every bot's status, kept current by the server's event stream. */
class BotFeed {
	statuses = $state<Record<string, BotStatus>>({});
	state = $state<FeedState>('idle');
	private source: EventSource | null = null;

	get ids(): string[] {
		return Object.keys(this.statuses).sort();
	}

	status(application: string): BotStatus | undefined {
		return this.statuses[application];
	}

	start(): void {
		if (this.source) return;
		this.state = 'connecting';
		const source = new EventSource(applications.eventsUrl, { withCredentials: true });
		source.onopen = () => {
			this.state = 'live';
		};
		source.onerror = () => {
			this.state = source.readyState === EventSource.CLOSED ? 'idle' : 'reconnecting';
		};
		source.addEventListener('bot', (event) => {
			const { application, ...status } = JSON.parse((event as MessageEvent).data) as BotEvent;
			this.statuses[application] = status;
		});
		this.source = source;
	}

	stop(): void {
		this.source?.close();
		this.source = null;
		this.state = 'idle';
		this.statuses = {};
	}

	/** Forgets an application that was removed. */
	forget(application: string): void {
		delete this.statuses[application];
	}

	/** Takes a status the API just returned, ahead of the stream. */
	put(application: string, status: BotStatus): void {
		this.statuses[application] = status;
	}
}

export const bots = new BotFeed();
