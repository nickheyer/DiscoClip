/** A shared "now" that ticks once a second, so relative times and countdowns stay honest. */
class Clock {
	now = $state(Date.now());
	private timer: ReturnType<typeof setInterval> | null = null;

	start(): void {
		if (this.timer) return;
		this.timer = setInterval(() => {
			this.now = Date.now();
		}, 1000);
	}

	stop(): void {
		if (this.timer) clearInterval(this.timer);
		this.timer = null;
	}
}

export const clock = new Clock();
