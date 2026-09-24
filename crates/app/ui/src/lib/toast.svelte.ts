// Toasts: the one toaster the shell mounts, and the ways pages speak through it.

import { createToaster } from '@skeletonlabs/skeleton-svelte';
import { ApiError } from './api/client';

export const toaster = createToaster({ placement: 'top-end', max: 5 });

const ERROR_DURATION = 8_000;

export const notify = {
	success(title: string, description?: string): void {
		toaster.success({ title, description });
	},
	error(title: string, description?: string): void {
		toaster.error({ title, description, duration: ERROR_DURATION });
	},
	info(title: string, description?: string): void {
		toaster.info({ title, description });
	}
};

/** Shows what went wrong. A rate limit counts down until the wait is over. */
export function reportError(error: unknown, title = 'That did not work'): void {
	if (error instanceof ApiError) {
		if (error.rateLimited && error.retryAfter !== null && error.retryAfter > 0) {
			countdown(title, error.retryAfter);
			return;
		}
		toaster.error({ title, description: error.message, duration: ERROR_DURATION });
		return;
	}
	const description = error instanceof Error ? error.message : String(error);
	toaster.error({ title, description, duration: ERROR_DURATION });
}

function countdown(title: string, seconds: number): void {
	const until = Date.now() + seconds * 1_000;
	const text = (left: number) => `Too many attempts. Try again in ${left} s.`;
	const id = toaster.create({
		type: 'error',
		title,
		description: text(seconds),
		duration: seconds * 1_000 + 3_000
	});
	const tick = setInterval(() => {
		const left = Math.max(0, Math.ceil((until - Date.now()) / 1_000));
		if (left <= 0) {
			clearInterval(tick);
			toaster.update(id, { type: 'info', description: 'You can try again now.' });
			return;
		}
		toaster.update(id, { description: text(left) });
	}, 1_000);
}
