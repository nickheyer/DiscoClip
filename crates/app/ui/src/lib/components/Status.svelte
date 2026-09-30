<script lang="ts" module>
	export type Tone = 'success' | 'error' | 'warning' | 'primary' | 'secondary' | 'surface';
</script>

<script lang="ts">
	import type {
		BotState,
		FixtureStatus,
		HealthStatus,
		JobStatus,
		SessionState
	} from '$lib/api/types';
	import { stageLabel } from '$lib/format';

	interface Props {
		job?: JobStatus;
		bot?: BotState;
		health?: HealthStatus;
		fixture?: FixtureStatus;
		session?: SessionState;
		/** On or off, shown as "On" and "Off". */
		enabled?: boolean;
		/** Whether a bot is in a server, shown as "In server" and "Left". */
		present?: boolean;
		/** Any other state, with the tone that colours it. */
		label?: string;
		tone?: Tone;
		pulse?: boolean;
		/** A longer explanation, shown on hover. */
		title?: string;
		class?: string;
	}

	let {
		job,
		bot,
		health,
		fixture,
		session,
		enabled,
		present,
		label,
		tone,
		pulse,
		title,
		class: className = ''
	}: Props = $props();

	const PRESET: Record<Tone, string> = {
		success: 'preset-tonal-success',
		error: 'preset-tonal-error',
		warning: 'preset-tonal-warning',
		primary: 'preset-tonal-primary',
		secondary: 'preset-tonal-secondary',
		surface: 'preset-tonal'
	};

	interface View {
		label: string;
		tone: Tone;
		pulse?: boolean;
		title?: string;
	}

	const view = $derived.by((): View => {
		if (job) {
			switch (job.status) {
				case 'queued':
					return { label: 'Queued', tone: 'surface' };
				case 'running':
					return { label: `Running · ${stageLabel(job.stage)}`, tone: 'primary', pulse: true };
				case 'done':
					return { label: 'Done', tone: 'success' };
				case 'failed':
					return { label: `Failed · ${stageLabel(job.stage)}`, tone: 'error', title: job.message };
				case 'cancelled':
					return { label: 'Cancelled', tone: 'warning' };
			}
		}
		if (bot) {
			switch (bot) {
				case 'disabled':
					return { label: 'No token', tone: 'surface' };
				case 'stopped':
					return { label: 'Stopped', tone: 'surface' };
				case 'starting':
					return { label: 'Starting', tone: 'primary', pulse: true };
				case 'connected':
					return { label: 'Connected', tone: 'success' };
				case 'retrying':
					return { label: 'Retrying', tone: 'warning', pulse: true };
				case 'failed':
					return { label: 'Failed', tone: 'error' };
			}
		}
		if (health) {
			switch (health) {
				case 'ok':
					return { label: 'OK', tone: 'success' };
				case 'warn':
					return { label: 'Warning', tone: 'warning' };
				case 'fail':
					return { label: 'Failing', tone: 'error' };
			}
		}
		if (fixture) {
			switch (fixture) {
				case 'pass':
					return { label: 'Pass', tone: 'success' };
				case 'fail':
					return { label: 'Fail', tone: 'error' };
				case 'login_required':
					return { label: 'Login required', tone: 'warning' };
				case 'never':
					return { label: 'Never run', tone: 'surface' };
			}
		}
		if (session) {
			switch (session) {
				case 'unsupported':
					return { label: 'No login', tone: 'surface' };
				case 'logged_out':
					return { label: 'Logged out', tone: 'warning' };
				case 'logged_in':
					return { label: 'Logged in', tone: 'success' };
			}
		}
		if (enabled !== undefined) {
			return enabled ? { label: 'On', tone: 'success' } : { label: 'Off', tone: 'surface' };
		}
		if (present !== undefined) {
			return present ? { label: 'In server', tone: 'success' } : { label: 'Left', tone: 'surface' };
		}
		return { label: label ?? '', tone: tone ?? 'surface', pulse, title };
	});
</script>

<!-- A Skeleton badge in the tone's tonal preset, with a dot that pulses while something runs. -->
<span
	class="badge {PRESET[view.tone]} {className}"
	style="--badge-size: var(--text-xs)"
	title={title ?? view.title}
>
	<span
		class="size-1.5 shrink-0 rounded-full bg-current {view.pulse ? 'animate-pulse' : ''}"
		aria-hidden="true"
	></span>
	{view.label}
</span>
