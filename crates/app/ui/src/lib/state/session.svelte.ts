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

	/**
	 * Goes to the login page first and forgets the account after, so no page that shows
	 * the account is left reading one that is gone. `next` is where to return after.
	 */
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

	/** Asks the server who the cookie belongs to; nobody when it answers 401. */
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
