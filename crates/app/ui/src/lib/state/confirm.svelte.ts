export interface ConfirmRequest {
	title: string;
	message: string;
	confirmLabel?: string;
	cancelLabel?: string;
	danger?: boolean;
	/** The word the person must type before confirming. */
	typed?: string;
}

interface Pending extends ConfirmRequest {
	resolve: (answer: boolean) => void;
}

class ConfirmState {
	pending = $state<Pending | null>(null);

	ask(request: ConfirmRequest): Promise<boolean> {
		this.pending?.resolve(false);
		return new Promise((resolve) => {
			this.pending = { ...request, resolve };
		});
	}

	answer(value: boolean): void {
		this.pending?.resolve(value);
		this.pending = null;
	}
}

export const confirm = new ConfirmState();
