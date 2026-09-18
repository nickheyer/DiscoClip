// What the platforms page says about a platform's fixtures and sessions.

import type {
	Found,
	FixtureStatus,
	MediaKind,
	PlatformCoverage,
	PlatformTag,
	SessionSupport
} from './api/types';
import { MEDIA_LABELS } from './api/types';
import { pluralize } from './format';

export const TAG_LABELS: Record<PlatformTag, { label: string; hint: string }> = {
	basic: { label: 'Basic', hint: 'The mainstream platforms most links in a chat point at.' },
	nsfw: { label: 'Adult', hint: 'Adult content, or a site that carries it as a matter of course.' },
	news: { label: 'News', hint: "News outlets and broadcasters' news programmes." },
	social: { label: 'Social', hint: 'Social networks and forums: posts by people.' },
	video: { label: 'Video', hint: 'General video sharing and hosting.' },
	music: { label: 'Music', hint: 'Music: tracks, albums, mixes.' },
	podcasts: { label: 'Podcasts', hint: 'Podcasts and spoken audio.' },
	live: { label: 'Live', hint: 'Live streaming, recordings of streams included.' },
	files: { label: 'Files', hint: 'File hosts and archives.' },
	images: { label: 'Images', hint: 'GIF and image hosts.' },
	players: { label: 'Players', hint: 'Embedded players and delivery services other sites build on.' }
};

export const MEDIA_HINTS: Record<MediaKind, string> = {
	video: 'Moving pictures, with or without sound; animated GIFs count.',
	audio: 'Sound alone: tracks, podcast episodes, sound bites.',
	image: 'Still pictures.',
	file: 'Any other file: documents, archives, binaries.'
};

/** What a fixture link resolved to, in a few words: "video · 3 variants", "playlist · 20 entries". */
export function describeFound(found: Found): string {
	if (found.kind === 'playlist') return `playlist · ${pluralize(found.entries, 'entry', 'entries')}`;
	return `${MEDIA_LABELS[found.media].toLowerCase()} · ${pluralize(found.variants, 'variant')}`;
}

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
	login_required: 'Needs login',
	never: 'Not run'
};

export type CoverageState = 'running' | 'passing' | 'failing' | 'login' | 'never' | 'none';

/**
 * Where a platform stands: running, every fixture passing, some failing, some resolving
 * only with a login its jar lacks, never run, or none to run.
 */
export function coverageState(platform: PlatformCoverage): CoverageState {
	if (platform.fixtures.length === 0) return 'none';
	if (platform.running) return 'running';
	if (platform.last_run_at === null) return 'never';
	if (platform.failed > 0) return 'failing';
	return platform.login_required > 0 ? 'login' : 'passing';
}

export const COVERAGE_LABELS: Record<CoverageState, string> = {
	running: 'Running',
	passing: 'All passing',
	failing: 'Failing',
	login: 'Needs login',
	never: 'Never run',
	none: 'No fixtures'
};

export function coverageTone(state: CoverageState): 'neutral' | 'ok' | 'warn' | 'danger' | 'info' {
	switch (state) {
		case 'passing':
			return 'ok';
		case 'failing':
			return 'danger';
		case 'login':
			return 'warn';
		case 'running':
			return 'info';
		default:
			return 'neutral';
	}
}

export function fixtureTone(status: FixtureStatus): 'neutral' | 'ok' | 'warn' | 'danger' {
	switch (status) {
		case 'pass':
			return 'ok';
		case 'fail':
			return 'danger';
		case 'login_required':
			return 'warn';
		default:
			return 'neutral';
	}
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
