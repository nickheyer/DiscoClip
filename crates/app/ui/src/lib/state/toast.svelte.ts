export type ToastTone = 'ok' | 'danger' | 'info';

export interface Toast {
	id: number;
	tone: ToastTone;
	message: string;
}

class ToastState {
	items = $state<Toast[]>([]);
	private next = 1;

	push(tone: ToastTone, message: string, ms = tone === 'danger' ? 8000 : 4000): void {
		const id = this.next++;
		this.items.push({ id, tone, message });
		setTimeout(() => this.dismiss(id), ms);
	}

	ok(message: string): void {
		this.push('ok', message);
	}

	error(message: string): void {
		this.push('danger', message);
	}

	info(message: string): void {
		this.push('info', message);
	}

	dismiss(id: number): void {
		this.items = this.items.filter((item) => item.id !== id);
	}
}

export const toast = new ToastState();
