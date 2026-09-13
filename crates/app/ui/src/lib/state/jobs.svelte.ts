import type { JobEvent, JobStats, JobSummary, LogEntry, Progress, Stage } from '$lib/api';
import type { FeedState } from '$lib/live';
import { live } from './live.svelte';

/** How many jobs the feed keeps around for pages that show what happened lately. */
const RECENT = 60;
/** How many log lines the feed keeps per job, for a detail page that watches it run. */
const LOG_LINES = 400;

export interface StageProgress {
	stage: Stage;
	progress: Progress;
	at: string;
}

type Listener = (event: JobEvent) => void;

/**
 * The engine as it runs: the latest counts and load, the jobs seen lately, each running
 * job's progress and log, all kept current by the server's live feed.
 */
class JobFeed {
	stats = $state<JobStats | null>(null);
	recent = $state<Record<string, JobSummary>>({});
	progress = $state<Record<string, StageProgress>>({});
	logs = $state<Record<string, LogEntry[]>>({});
	private off: (() => void) | null = null;
	private listeners = new Set<Listener>();

	get state(): FeedState {
		return live.state;
	}

	/** The jobs seen lately, newest first. */
	get recentJobs(): JobSummary[] {
		return Object.values(this.recent).sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
	}

	summary(job: string): JobSummary | undefined {
		return this.recent[job];
	}

	start(): void {
		if (this.off) return;
		this.off = live.on((name, data) => {
			if (name === 'stats') this.stats = JSON.parse(data) as JobStats;
			else if (name === 'job') this.take(JSON.parse(data) as JobEvent);
		});
		live.start();
	}

	stop(): void {
		if (!this.off) return;
		this.off();
		this.off = null;
		live.stop();
		this.stats = null;
		this.recent = {};
		this.progress = {};
		this.logs = {};
	}

	/** Runs `listener` for every job event until the returned function is called. */
	subscribe(listener: Listener): () => void {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	}

	/** Takes summaries the API returned, so a page's list and the feed agree. */
	seed(list: JobSummary[]): void {
		for (const job of list) this.recent[job.id] = job;
		this.trim();
	}

	/** Takes stats the API returned ahead of the stream. */
	seedStats(stats: JobStats): void {
		if (!this.stats || this.stats.at < stats.at) this.stats = stats;
	}

	forget(job: string): void {
		delete this.recent[job];
		delete this.progress[job];
		delete this.logs[job];
	}

	private take(event: JobEvent): void {
		if (event.job_summary) this.recent[event.job] = event.job_summary;
		switch (event.kind) {
			case 'progress':
				this.progress[event.job] = { stage: event.stage, progress: event.progress, at: event.at };
				break;
			case 'status':
				if (event.status.status !== 'running') delete this.progress[event.job];
				break;
			case 'log': {
				const lines = this.logs[event.job] ?? [];
				lines.push(event.entry);
				if (lines.length > LOG_LINES) lines.splice(0, lines.length - LOG_LINES);
				this.logs[event.job] = lines;
				break;
			}
			case 'deleted':
				this.forget(event.job);
				break;
			default:
				break;
		}
		this.trim();
		for (const listener of this.listeners) listener(event);
	}

	private trim(): void {
		const ids = Object.keys(this.recent);
		if (ids.length <= RECENT) return;
		const oldest = ids
			.map((id) => this.recent[id]!)
			.sort((a, b) => (a.created_at < b.created_at ? -1 : 1))
			.slice(0, ids.length - RECENT);
		for (const job of oldest) {
			if (job.status.status === 'queued' || job.status.status === 'running') continue;
			delete this.recent[job.id];
			delete this.logs[job.id];
		}
	}
}

export const jobs = new JobFeed();
