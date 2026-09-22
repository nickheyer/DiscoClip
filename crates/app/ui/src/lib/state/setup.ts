import { auth } from '$lib/api';
// Cache setup completion once the first admin exists.
let needed: boolean | null = null;

export async function setupNeeded(): Promise<boolean> {
	if (needed === false) return false;
	needed = (await auth.setupStatus()).needed;
	return needed;
}

export function setupDone(): void {
	needed = false;
}
