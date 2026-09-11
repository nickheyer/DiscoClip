import { auth } from '$lib/api';

// Whether the first admin still has to be created. Once it exists it never has to again,
// so the answer is asked for only until it is `false`.
let needed: boolean | null = null;

export async function setupNeeded(): Promise<boolean> {
	if (needed === false) return false;
	needed = (await auth.setupStatus()).needed;
	return needed;
}

export function setupDone(): void {
	needed = false;
}
