import { goto } from '$app/navigation';
import { auth, isApiError, setCsrfToken } from '$lib/api';
import type { Permission, Role, User, WhoAmI } from '$lib/api';
import { roleAllows } from '$lib/permissions';

class SessionState {
	me = $state<WhoAmI | null>(null);
	loaded = $state(false);
	/** Set while leaving for the login page, so the layout lets a still-known account in. */
	ending = false;

	get user(): User | null {
		return this.me?.user ?? null;
	}

	get role(): Role | null {
		return this.me?.user.role ?? null;
	}

	can(permission: Permission): boolean {
		const role = this.role;
		return role !== null && roleAllows(role, permission);
	}

	set(me: WhoAmI | null): void {
		this.me = me;
		this.loaded = true;
		setCsrfToken(me?.csrf_token ?? null);
	}

	clear(): void {
		this.set(null);
	}

	/** Navigate before clearing the session so mounted pages retain their account data. */
	async leave(next?: string): Promise<void> {
		this.ending = true;
		try {
			await goto(
				next && next !== '/' ? `/login?next=${encodeURIComponent(next)}` : '/login',
				{ replaceState: false }
			);
		} finally {
			this.set(null);
			this.ending = false;
		}
	}

	/** Asks the server who the cookie belongs to. Nobody when it answers 401. */
	async load(): Promise<WhoAmI | null> {
		try {
			this.set(await auth.session());
		} catch (error) {
			if (isApiError(error) && error.status === 401) {
				this.set(null);
			} else {
				throw error;
			}
		}
		return this.me;
	}
}

export const session = new SessionState();
