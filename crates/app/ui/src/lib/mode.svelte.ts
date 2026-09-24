// The light or dark mode in force, switched from the account menu and public view headers.

type Mode = 'light' | 'dark';

class ModeState {
	current = $state<Mode>(
		typeof document !== 'undefined' && document.documentElement.dataset.mode === 'dark'
			? 'dark'
			: 'light'
	);

	/** The mode a switch would move to, which is what its label names. */
	get target(): Mode {
		return this.current === 'dark' ? 'light' : 'dark';
	}

	get label(): string {
		return this.target === 'light' ? 'Light mode' : 'Dark mode';
	}

	get action(): string {
		return `Switch to ${this.target} mode`;
	}

	toggle() {
		this.current = this.target;
		document.documentElement.dataset.mode = this.current;
		try {
			localStorage.setItem('discoclip.mode', this.current);
		} catch {
			// Storage is unavailable; the choice lasts for this page.
		}
	}
}

export const mode = new ModeState();
