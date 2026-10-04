// A clock every relative time on the page follows, so they all refresh together.

const TICK = 30_000;

class Now {
	value = $state(Date.now());
	#timer: ReturnType<typeof setInterval> | null = null;

	/** Starts ticking on the first call. */
	start(): void {
		if (this.#timer !== null || typeof window === 'undefined') return;
		this.#timer = setInterval(() => {
			this.value = Date.now();
		}, TICK);
	}
}

export const now = new Now();
