// How the health and metrics pages show status and amounts.

import type { HealthStatus } from './api/types';

export const HEALTH_LABELS: Record<HealthStatus, string> = {
	ok: 'Healthy',
	warn: 'Needs a look',
	fail: 'Failing'
};

export function healthTone(status: HealthStatus): 'ok' | 'warn' | 'danger' {
	return status === 'ok' ? 'ok' : status === 'warn' ? 'warn' : 'danger';
}

export function healthIcon(status: HealthStatus): string {
	return status === 'ok' ? 'check-circle' : status === 'warn' ? 'warning' : 'x-circle';
}

/** `3d 4h`, `2h 05m`, `48s`: how long something has been up. */
export function formatUptime(totalSeconds: number): string {
	const seconds = Math.max(0, Math.round(totalSeconds));
	const d = Math.floor(seconds / 86400);
	const h = Math.floor((seconds % 86400) / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	const s = seconds % 60;
	if (d > 0) return `${d}d ${h}h`;
	if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m`;
	if (m > 0) return `${m}m ${String(s).padStart(2, '0')}s`;
	return `${s}s`;
}

export function percent(part: number, whole: number): number {
	return whole > 0 ? Math.round((part / whole) * 100) : 0;
}
