// Who is logged in, what they may do, and the way in and out.

import { goto } from '$app/navigation';
import { resolve } from '$app/paths';
import { onUnauthorized, setCsrfToken, ApiError } from './api/client';
import { auth } from './api/endpoints';
import { ROLE_PERMISSIONS, type Permission, type User, type WhoAmI } from './api/types';

class SessionStore {
	/** The account and session, or nothing while logged out. */
	who = $state<WhoAmI | null>(null);
	/** Whether the first `refresh()` has answered. */
	loaded = $state(false);

	get user(): User | null {
		return this.who?.user ?? null;
	}

	/** Whether the account's role grants a permission. */
	can(permission: Permission): boolean {
		const role = this.who?.user.role;
		return role !== undefined && ROLE_PERMISSIONS[role].includes(permission);
	}

	#accept(who: WhoAmI | null): void {
		this.who = who;
		setCsrfToken(who?.csrf_token ?? null);
	}

	/** Asks the server who holds the session cookie. */
	async refresh(): Promise<WhoAmI | null> {
		try {
			this.#accept(await auth.session({ expectUnauthorized: true }));
		} catch (error) {
			if (error instanceof ApiError && error.unauthorized) {
				this.#accept(null);
			} else {
				throw error;
			}
		} finally {
			this.loaded = true;
		}
		return this.who;
	}

	async login(username: string, password: string): Promise<WhoAmI> {
		const who = await auth.login({ username, password });
		this.#accept(who);
		this.loaded = true;
		return who;
	}

	async setup(username: string, password: string): Promise<WhoAmI> {
		const who = await auth.setup({ username, password });
		this.#accept(who);
		this.loaded = true;
		return who;
	}

	async recover(username: string, key: string, password: string): Promise<WhoAmI> {
		const who = await auth.recover({ username, key, password });
		this.#accept(who);
		this.loaded = true;
		return who;
	}

	async logout(): Promise<void> {
		try {
			await auth.logout();
		} finally {
			this.#accept(null);
			await goto(resolve('/login'));
		}
	}

	/** Forgets the session and sends the browser to the login page, to come back to `next`. */
	async expire(): Promise<void> {
		this.#accept(null);
		await this.goToLogin(location.pathname + location.search);
	}

	/** Opens the login page, which returns to `next` after a login. */
	async goToLogin(next?: string): Promise<void> {
		if (location.pathname === '/login') return;
		const login = resolve('/login');
		if (!next || next === '/') {
			await goto(login);
			return;
		}
		// The query rides on the resolved path, which is what the rule is there to secure.
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		await goto(`${login}?next=${encodeURIComponent(next)}`);
	}
}

export const session = new SessionStore();

onUnauthorized(() => {
	void session.expire();
});
