export type ThemeMode = 'system' | 'light' | 'dark';

const KEY = 'discoclip.theme';

function read(): ThemeMode {
	try {
		const stored = localStorage.getItem(KEY);
		if (stored === 'light' || stored === 'dark') return stored;
	} catch {
		// Storage can be unavailable; the system setting applies.
	}
	return 'system';
}

class ThemeState {
	mode = $state<ThemeMode>('system');

	init(): void {
		this.mode = read();
		this.apply();
	}

	set(mode: ThemeMode): void {
		this.mode = mode;
		try {
			if (mode === 'system') localStorage.removeItem(KEY);
			else localStorage.setItem(KEY, mode);
		} catch {
			// Storage can be unavailable; the choice lasts for this page.
		}
		this.apply();
	}

	cycle(): void {
		const order: ThemeMode[] = ['system', 'light', 'dark'];
		this.set(order[(order.indexOf(this.mode) + 1) % order.length]!);
	}

	private apply(): void {
		const root = document.documentElement;
		if (this.mode === 'system') delete root.dataset.theme;
		else root.dataset.theme = this.mode;
	}
}

export const theme = new ThemeState();
