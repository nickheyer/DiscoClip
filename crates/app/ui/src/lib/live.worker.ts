/// <reference lib="webworker" />
// One connection to the server's live feed for every tab of the app in this browser. The
// tabs connect to this worker, which holds the event stream, relays everything it
// carries, and hands a tab that connects later the latest counts and bot statuses, as a
// fresh stream would. A browser allows a host only a few connections at a time, so a
// stream per tab would leave none for pages, downloads and video once a few tabs are open.

import { LIVE_EVENTS_URL } from '$lib/api/endpoints';
import { LIVE_EVENT_NAMES } from '$lib/live';
import type { FeedState, LiveCommand, LiveEventName, LiveMessage } from '$lib/live';

declare const self: SharedWorkerGlobalScope;

const ports = new Set<MessagePort>();
let source: EventSource | null = null;
let state: FeedState = 'idle';
/** The latest counts and the latest status of each bot, replayed to a tab that connects. */
const latest: { stats: string | null; bots: Map<string, string> } = { stats: null, bots: new Map() };

function send(port: MessagePort, message: LiveMessage): void {
	port.postMessage(message);
}

function broadcast(message: LiveMessage): void {
	for (const port of ports) send(port, message);
}

function setState(next: FeedState): void {
	state = next;
	broadcast({ type: 'state', state });
}

function remember(name: LiveEventName, data: string): void {
	if (name === 'stats') latest.stats = data;
	if (name === 'bot') {
		const { application, removed } = JSON.parse(data) as { application: string; removed?: true };
		if (removed) latest.bots.delete(application);
		else latest.bots.set(application, data);
	}
}

function open(): void {
	if (source) return;
	setState('connecting');
	const stream = new EventSource(LIVE_EVENTS_URL, { withCredentials: true });
	stream.onopen = () => setState('live');
	stream.onerror = () => setState(stream.readyState === EventSource.CLOSED ? 'idle' : 'reconnecting');
	for (const name of LIVE_EVENT_NAMES) {
		stream.addEventListener(name, (event) => {
			const data = (event as MessageEvent<string>).data;
			remember(name, data);
			broadcast({ type: 'event', name, data });
		});
	}
	source = stream;
}

function close(): void {
	source?.close();
	source = null;
	latest.stats = null;
	latest.bots.clear();
	setState('idle');
}

/** Brings a tab up to date with what the stream has said so far. */
function replay(port: MessagePort): void {
	send(port, { type: 'state', state });
	if (latest.stats) send(port, { type: 'event', name: 'stats', data: latest.stats });
	for (const data of latest.bots.values()) send(port, { type: 'event', name: 'bot', data });
}

self.onconnect = (event: MessageEvent) => {
	const port = event.ports[0]!;
	port.onmessage = (message: MessageEvent<LiveCommand>) => {
		if (message.data === 'start') {
			ports.add(port);
			// A stream that ended, as after a logout, is opened again for the next start.
			if (source && state === 'idle') close();
			open();
			replay(port);
		} else if (message.data === 'stop') {
			ports.delete(port);
			if (ports.size === 0) close();
		}
	};
};
