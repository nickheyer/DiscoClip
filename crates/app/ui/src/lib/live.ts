// Keeping a list of job summaries current from the feed's job events.

import type { JobEvent, JobSummary } from './api/types';

export interface MergeOptions {
	/** Add jobs the list has not seen, at the top. */
	insert?: boolean;
	/** Which jobs belong in the list at all. */
	filter?: (job: JobSummary) => boolean;
	/** Drop rows past this many after an insert. */
	max?: number;
}

/** The list after one job event: rows replaced, removed or added as the event says. */
export function mergeJobEvent(
	rows: JobSummary[],
	event: JobEvent,
	options: MergeOptions = {}
): JobSummary[] {
	if (event.kind === 'deleted') {
		return rows.filter((row) => row.id !== event.job);
	}
	const summary = event.job_summary;
	if (!summary) return rows;
	const keep = options.filter ? options.filter(summary) : true;
	const index = rows.findIndex((row) => row.id === summary.id);
	if (index >= 0) {
		if (!keep) return rows.filter((row) => row.id !== summary.id);
		const next = rows.slice();
		next[index] = summary;
		return next;
	}
	if (!options.insert || !keep) return rows;
	const next = [summary, ...rows];
	return options.max !== undefined && next.length > options.max ? next.slice(0, options.max) : next;
}
