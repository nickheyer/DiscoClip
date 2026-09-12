// What the platforms page says about a platform's fixtures and sessions.

import type { FixtureStatus, PlatformCoverage, SessionSupport } from './api/types';

export const SESSION_LABELS: Record<SessionSupport, { label: string; hint: string }> = {
	none: { label: 'No account', hint: 'The platform is read without an account.' },
	optional: {
		label: 'Account optional',
		hint: 'Public links work without one; a session unlocks age-gated, private or higher quality media.'
	},
	required: { label: 'Account needed', hint: 'Nothing resolves without a logged-in session.' }
};

export const FIXTURE_STATUS_LABELS: Record<FixtureStatus, string> = {
	pass: 'Passing',
	fail: 'Failing',
	never: 'Not run'
};

export type CoverageState = 'running' | 'passing' | 'failing' | 'never' | 'none';

/** Where a platform stands: running, every fixture passing, some failing, never run, or none to run. */
export function coverageState(platform: PlatformCoverage): CoverageState {
	if (platform.fixtures.length === 0) return 'none';
	if (platform.running) return 'running';
	if (platform.last_run_at === null) return 'never';
	return platform.failed === 0 ? 'passing' : 'failing';
}

export const COVERAGE_LABELS: Record<CoverageState, string> = {
	running: 'Running',
	passing: 'All passing',
	failing: 'Failing',
	never: 'Never run',
	none: 'No fixtures'
};

export function coverageTone(state: CoverageState): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' {
	switch (state) {
		case 'passing':
			return 'ok';
		case 'failing':
			return 'danger';
		case 'running':
			return 'info';
		default:
			return 'neutral';
	}
}

export function fixtureTone(status: FixtureStatus): 'neutral' | 'ok' | 'danger' {
	return status === 'pass' ? 'ok' : status === 'fail' ? 'danger' : 'neutral';
}

/** What a platform's session amounts to, from its cookies and what it last said. */
export function sessionSummary(platform: PlatformCoverage): {
	label: string;
	tone: 'neutral' | 'ok' | 'warn' | 'info';
} {
	if (platform.session === 'none') return { label: 'No account needed', tone: 'neutral' };
	const check = platform.session_check;
	if (check?.state === 'logged_in') return { label: `Logged in as ${check.account}`, tone: 'ok' };
	if (check?.state === 'unsupported') return { label: 'Sessions not checked here', tone: 'neutral' };
	if (platform.cookies === 0) {
		return platform.session === 'required'
			? { label: 'No cookies; nothing resolves', tone: 'warn' }
			: { label: 'No cookies', tone: 'neutral' };
	}
	if (check?.state === 'logged_out') return { label: 'Cookies stored but logged out', tone: 'warn' };
	return { label: 'Cookies stored, not checked', tone: 'info' };
}
