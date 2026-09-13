import type { BotEvent, BotStatus } from '$lib/api';
import type { FeedState } from '$lib/live';
import { live } from './live.svelte';

export type { FeedState } from '$lib/live';

/** Every bot's status, kept current by the server's live feed. */
class BotFeed {
	statuses = $state<Record<string, BotStatus>>({});
	private off: (() => void) | null = null;

	get state(): FeedState {
		return live.state;
	}

	get ids(): string[] {
		return Object.keys(this.statuses).sort();
	}

	status(application: string): BotStatus | undefined {
		return this.statuses[application];
	}

	start(): void {
		if (this.off) return;
		this.off = live.on((name, data) => {
			if (name !== 'bot') return;
			const { application, removed, ...status } = JSON.parse(data) as BotEvent;
			if (removed) delete this.statuses[application];
			else this.statuses[application] = status;
		});
		live.start();
	}

	stop(): void {
		if (!this.off) return;
		this.off();
		this.off = null;
		live.stop();
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
