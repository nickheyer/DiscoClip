// Every setting the server has, as the settings page edits it: its dotted key, what it
// holds, and how it is shown. The keys mirror the `Settings` type on the server one for one.

import type { SettingValue } from '$lib/api';

export type FieldKind =
	| 'text'
	| 'path'
	| 'url'
	| 'socket'
	| 'integer'
	| 'number'
	| 'boolean'
	| 'secret'
	| 'list'
	| 'enum'
	| 'bytes'
	| 'seconds'
	| 'days'
	| 'millis';

export interface FieldSpec {
	/** The last segment of the key, under the section's key. */
	name: string;
	label: string;
	kind: FieldKind;
	hint: string;
	/** For `enum`. */
	options?: { value: string; label: string }[];
	/** Whether `null` is a value of its own: no limit, nothing set. */
	nullable?: boolean;
	/** What an empty field means when the value may be null. */
	nullLabel?: string;
	/** Widest value accepted, for numbers. */
	min?: number;
	placeholder?: string;
	/** For `list`: a message when an item is not acceptable, nothing when it is. */
	validate?: (item: string) => string | null;
}

/** A map keyed by a host name, held whole at one key. */
export interface MapSpec {
	key: string;
	label: string;
	hint: string;
	keyLabel: string;
	keyPlaceholder: string;
	/** What each entry holds. */
	fields: FieldSpec[];
	/** When there is one field, the map's values are its values rather than objects. */
	scalar: boolean;
}

export interface SectionSpec {
	/** The dotted key of the section; fields live under it. */
	key: string;
	title: string;
	description: string;
	icon: string;
	fields: FieldSpec[];
	/** Sections beneath this one. */
	sections?: SectionSpec[];
	/** Maps beneath this one. */
	maps?: MapSpec[];
	/**
	 * The section is one optional object: absent as a whole when off, and every field
	 * present when on.
	 */
	optional?: {
		label: string;
		hint: string;
	};
}

export const HOST_RE = /^(?=.{1,253}$)(\*\.)?([a-z0-9-]+\.)+[a-z0-9-]+$/i;
export const NETWORK_RE =
	/^(\d{1,3}(\.\d{1,3}){3}(\/\d{1,2})?|[0-9a-f:]+(\/\d{1,3})?)$/i;

const hostProblem = (item: string) => (HOST_RE.test(item) ? null : `${item} is not a host name`);
const networkProblem = (item: string) =>
	NETWORK_RE.test(item) ? null : `${item} is not an address or a network in CIDR form`;
const scopeProblem = (item: string) =>
	/^[\w.:/-]+$/.test(item) ? null : `${item} is not an OAuth scope`;

export const SECTIONS: SectionSpec[] = [
	{
		key: 'log',
		title: 'Logging',
		description: 'What the server writes to its log.',
		icon: 'file-text',
		fields: [
			{
				name: 'level',
				label: 'Filter',
				kind: 'text',
				hint: 'A tracing filter: a level such as info, or per-crate directives such as info,discoclip_engine=debug. Applies at once.',
				placeholder: 'info'
			}
		]
	},
	{
		key: 'engine',
		title: 'Engine',
		description: 'How jobs are run: where files go, how many run at once, and what is refused.',
		icon: 'zap',
		fields: [
			{
				name: 'cache_dir',
				label: 'Cache directory',
				kind: 'path',
				hint: 'Where ffmpeg is unpacked and in-flight downloads and finished outputs are kept. Jobs from now on use a new directory; running ones finish where they started.'
			},
			{
				name: 'workers',
				label: 'Workers',
				kind: 'integer',
				hint: 'Jobs processed at the same time. The pool grows at once and shrinks as running jobs finish.',
				min: 1
			}
		],
		sections: [
			{
				key: 'engine.limits',
				title: 'Limits',
				description: 'What a link is refused for. Watch rules and submissions can tighten these, never loosen them.',
				icon: 'shield',
				fields: [
					{
						name: 'max_source_bytes',
						label: 'Largest source',
						kind: 'bytes',
						hint: 'The biggest download accepted.',
						min: 1
					},
					{
						name: 'max_duration_secs',
						label: 'Longest video',
						kind: 'seconds',
						hint: 'Videos longer than this are refused. Empty means no limit; zero refuses live streams and accepts nothing else.',
						nullable: true,
						nullLabel: 'No limit',
						min: 0
					},
					{
						name: 'max_height',
						label: 'Tallest output',
						kind: 'integer',
						hint: 'In pixels; taller sources are downscaled.',
						min: 1
					}
				]
			},
			{
				key: 'engine.archive',
				title: 'Archive',
				description: 'Keep a copy of every finished video beside a record of its job.',
				icon: 'download',
				optional: {
					label: 'Archive finished videos',
					hint: 'Off, nothing is kept beyond the cache and what was posted.'
				},
				fields: [
					{
						name: 'dir',
						label: 'Archive directory',
						kind: 'path',
						hint: 'Files go under year and month folders here.',
						placeholder: 'archive'
					},
					{
						name: 'keep',
						label: 'Keep',
						kind: 'enum',
						hint: 'Which files are copied into the archive.',
						options: [
							{ value: 'output', label: 'The output' },
							{ value: 'source', label: 'The source' },
							{ value: 'both', label: 'Both' }
						]
					}
				]
			},
			{
				key: 'engine.playlists',
				title: 'Playlists',
				description: 'Links to playlists and channels expand into one job per entry.',
				icon: 'rules',
				fields: [
					{
						name: 'enabled',
						label: 'Expand playlists',
						kind: 'boolean',
						hint: 'Off, a playlist link fails as one.'
					},
					{
						name: 'max_entries',
						label: 'Most entries',
						kind: 'integer',
						hint: 'The most entries one playlist link expands into.',
						min: 1
					}
				]
			},
			{
				key: 'engine.live',
				title: 'Live streams',
				description: 'A live stream is captured from the moment the link is seen.',
				icon: 'activity',
				fields: [
					{
						name: 'max_capture_secs',
						label: 'Longest capture',
						kind: 'seconds',
						hint: 'How long a live stream is recorded before it is cut and treated as a recording.',
						min: 0
					}
				]
			},
			{
				key: 'engine.retention',
				title: 'Retention',
				description: 'How long finished jobs and their cached files are kept.',
				icon: 'clock',
				fields: [
					{
						name: 'jobs_days',
						label: 'Finished jobs',
						kind: 'days',
						hint: 'Finished jobs older than this are removed; zero keeps them forever.',
						min: 0
					},
					{
						name: 'failed_jobs_days',
						label: 'Failed and cancelled jobs',
						kind: 'days',
						hint: 'Failed and cancelled jobs older than this are removed; zero keeps them forever.',
						min: 0
					},
					{
						name: 'cache_max_bytes',
						label: 'Cache size',
						kind: 'bytes',
						hint: 'The cache directory is trimmed back under this, oldest jobs first; zero never trims.',
						min: 0
					},
					{
						name: 'sweep_interval_secs',
						label: 'Sweep every',
						kind: 'seconds',
						hint: 'How often retention runs.',
						min: 1
					}
				]
			}
		]
	},
	{
		key: 'http',
		title: 'HTTP',
		description: 'How resolvers and downloaders reach the platforms.',
		icon: 'globe',
		fields: [
			{
				name: 'user_agent',
				label: 'User agent',
				kind: 'text',
				hint: 'Sent when a request names no other user agent.'
			},
			{
				name: 'connect_timeout_secs',
				label: 'Connect timeout',
				kind: 'seconds',
				hint: 'How long to wait for a connection to a host.',
				min: 1
			},
			{
				name: 'request_timeout_secs',
				label: 'Request timeout',
				kind: 'seconds',
				hint: 'Bound on a whole page or API request; media downloads have none.',
				min: 1
			},
			{
				name: 'read_timeout_secs',
				label: 'Read timeout',
				kind: 'seconds',
				hint: 'Longest pause between two chunks of a body.',
				min: 1
			},
			{
				name: 'max_redirects',
				label: 'Redirects followed',
				kind: 'integer',
				hint: 'How many redirects a request follows before giving up.',
				min: 0
			}
		],
		sections: [
			{
				key: 'http.retry',
				title: 'Retries',
				description: 'Requests that fail for reasons that pass are asked again with exponential backoff.',
				icon: 'restart',
				fields: [
					{
						name: 'attempts',
						label: 'Attempts',
						kind: 'integer',
						hint: 'Total attempts, including the first.',
						min: 1
					},
					{
						name: 'base_ms',
						label: 'First pause',
						kind: 'millis',
						hint: 'The pause before the first retry; each retry doubles it.',
						min: 0
					},
					{
						name: 'max_ms',
						label: 'Longest pause',
						kind: 'millis',
						hint: 'The pause is capped here.',
						min: 0
					}
				]
			},
			{
				key: 'http.rate_limits',
				title: 'Rate limits',
				description: 'Requests to one host are paced so a burst of links does not get the server throttled or banned there.',
				icon: 'clock',
				fields: [],
				sections: [
					{
						key: 'http.rate_limits.default',
						title: 'Default rate',
						description: 'For every host without a rate of its own. Zero per second means no limit.',
						icon: 'clock',
						fields: [
							{
								name: 'per_second',
								label: 'Requests per second',
								kind: 'number',
								hint: 'On average; zero means no limit.',
								min: 0
							},
							{
								name: 'burst',
								label: 'Burst',
								kind: 'integer',
								hint: 'How many may go at once when the host has been idle.',
								min: 0
							}
						]
					}
				],
				maps: [
					{
						key: 'http.rate_limits.hosts',
						label: 'Per host',
						hint: 'Rates by host suffix: youtube.com covers www.youtube.com. The longest matching suffix wins.',
						keyLabel: 'Host',
						keyPlaceholder: 'youtube.com',
						scalar: false,
						fields: [
							{
								name: 'per_second',
								label: 'Per second',
								kind: 'number',
								hint: '',
								min: 0
							},
							{ name: 'burst', label: 'Burst', kind: 'integer', hint: '', min: 0 }
						]
					}
				]
			},
			{
				key: 'http.proxies',
				title: 'Proxies',
				description: 'HTTP and SOCKS proxies, by platform and by host suffix, over a default; bypassed hosts go direct.',
				icon: 'server',
				fields: [
					{
						name: 'default',
						label: 'Default proxy',
						kind: 'url',
						hint: 'http://, https://, socks5://, socks5h://, socks4:// or socks4a://. Empty means direct.',
						nullable: true,
						nullLabel: 'Direct',
						placeholder: 'socks5h://proxy:1080'
					},
					{
						name: 'bypass',
						label: 'Bypass',
						kind: 'list',
						hint: 'Host suffixes reached without any proxy.',
						validate: hostProblem,
						placeholder: 'internal.example'
					}
				],
				maps: [
					{
						key: 'http.proxies.platforms',
						label: 'By platform',
						hint: 'A proxy for every request a platform’s resolver makes, such as tiktok or youtube.',
						keyLabel: 'Platform',
						keyPlaceholder: 'tiktok',
						scalar: true,
						fields: [
							{
								name: 'url',
								label: 'Proxy URL',
								kind: 'url',
								hint: '',
								placeholder: 'socks5h://proxy:1080'
							}
						]
					},
					{
						key: 'http.proxies.hosts',
						label: 'By host',
						hint: 'A proxy by host suffix; the longest matching suffix wins.',
						keyLabel: 'Host',
						keyPlaceholder: 'googlevideo.com',
						scalar: true,
						fields: [
							{
								name: 'url',
								label: 'Proxy URL',
								kind: 'url',
								hint: '',
								placeholder: 'http://proxy:3128'
							}
						]
					}
				]
			}
		]
	},
	{
		key: 'local',
		title: 'Local publishing',
		description: 'Where jobs submitted from this web app are written.',
		icon: 'download',
		fields: [
			{
				name: 'dir',
				label: 'Directory',
				kind: 'path',
				hint: 'Finished videos of jobs submitted here are copied into this directory.'
			},
			{
				name: 'max_bytes',
				label: 'Largest output',
				kind: 'bytes',
				hint: 'The size budget a submitted job is transcoded to fit.',
				min: 1
			}
		]
	},
	{
		key: 'fixtures',
		title: 'Platform fixtures',
		description:
			'Every platform names public links its fixtures resolve, so the platforms page shows what works and when each platform last passed in full.',
		icon: 'check-circle',
		fields: [
			{
				name: 'interval_secs',
				label: 'Run every',
				kind: 'seconds',
				hint: 'How often every platform’s fixtures run on their own. Zero runs them only from the platforms page.',
				min: 0
			},
			{
				name: 'timeout_secs',
				label: 'Link timeout',
				kind: 'seconds',
				hint: 'The longest one link may take to resolve before it counts as failed.',
				min: 1
			}
		]
	},
	{
		key: 'web',
		title: 'Web app',
		description: 'How this app is reached.',
		icon: 'monitor',
		fields: [
			{
				name: 'bind',
				label: 'Listen on',
				kind: 'socket',
				hint: 'Address and port, such as 127.0.0.1:8080 or [::]:8080. The app listens again on the new address at once; this page follows.',
				placeholder: '127.0.0.1:8080'
			},
			{
				name: 'public_url',
				label: 'Public URL',
				kind: 'url',
				hint: 'How browsers reach the app; login providers send them back here. Empty, the address a request arrived at is used.',
				nullable: true,
				nullLabel: 'From each request',
				placeholder: 'https://clips.example.com'
			},
			{
				name: 'trusted_proxies',
				label: 'Trusted proxies',
				kind: 'list',
				hint: 'Addresses and networks whose Forwarded and X-Forwarded-* headers are believed.',
				validate: networkProblem,
				placeholder: '10.0.0.0/8'
			}
		],
		sections: [
			{
				key: 'web.tls',
				title: 'HTTPS',
				description: 'Serve HTTPS from the binary. The files are read again when they change, so a renewed certificate needs nothing here.',
				icon: 'lock',
				optional: {
					label: 'Serve HTTPS',
					hint: 'Off, the app speaks plain HTTP, as it does behind a reverse proxy that terminates TLS.'
				},
				fields: [
					{
						name: 'cert',
						label: 'Certificate chain',
						kind: 'path',
						hint: 'A PEM file.',
						placeholder: '/etc/discoclip/fullchain.pem'
					},
					{
						name: 'key',
						label: 'Private key',
						kind: 'path',
						hint: 'A PEM file.',
						placeholder: '/etc/discoclip/privkey.pem'
					}
				]
			}
		]
	},
	{
		key: 'auth',
		title: 'Login providers',
		description: 'Ways to log in besides a password. Register an application at the provider with the callback /api/auth/<provider>/callback under the public URL. Discord login is offered by the Discord application marked for it.',
		icon: 'key',
		fields: [
			{
				name: 'oauth_signup',
				label: 'Sign-up through providers',
				kind: 'boolean',
				hint: 'Create a viewer account for a provider identity nobody has linked, at its first login.'
			}
		],
		sections: [
			{
				key: 'auth.github',
				title: 'GitHub',
				description: 'An OAuth app registered at GitHub.',
				icon: 'github',
				optional: { label: 'Offer GitHub login', hint: '' },
				fields: [
					{ name: 'client_id', label: 'Client ID', kind: 'text', hint: '' },
					{ name: 'client_secret', label: 'Client secret', kind: 'secret', hint: '' }
				]
			},
			{
				key: 'auth.google',
				title: 'Google',
				description: 'An OAuth client registered in Google Cloud.',
				icon: 'google',
				optional: { label: 'Offer Google login', hint: '' },
				fields: [
					{ name: 'client_id', label: 'Client ID', kind: 'text', hint: '' },
					{ name: 'client_secret', label: 'Client secret', kind: 'secret', hint: '' }
				]
			},
			{
				key: 'auth.oidc',
				title: 'OpenID Connect',
				description: 'Any OpenID Connect issuer, found through its discovery document.',
				icon: 'key',
				optional: { label: 'Offer single sign-on', hint: '' },
				fields: [
					{
						name: 'name',
						label: 'Button label',
						kind: 'text',
						hint: 'What the login button says.',
						placeholder: 'Single sign-on'
					},
					{
						name: 'issuer',
						label: 'Issuer',
						kind: 'url',
						hint: 'The issuer URL; /.well-known/openid-configuration is read beneath it.',
						placeholder: 'https://login.example.com/realms/main'
					},
					{ name: 'client_id', label: 'Client ID', kind: 'text', hint: '' },
					{ name: 'client_secret', label: 'Client secret', kind: 'secret', hint: '' },
					{
						name: 'scopes',
						label: 'Scopes',
						kind: 'list',
						hint: 'Asked for at login; openid is needed.',
						validate: scopeProblem,
						placeholder: 'openid'
					}
				]
			}
		]
	}
];

/** Every section, this one and the ones beneath it, depth first. */
export function flatten(sections: SectionSpec[]): SectionSpec[] {
	const out: SectionSpec[] = [];
	for (const section of sections) {
		out.push(section);
		if (section.sections) out.push(...flatten(section.sections));
	}
	return out;
}

export const ALL_SECTIONS = flatten(SECTIONS);

/** The section a dotted key belongs to, the deepest one. */
export function sectionOf(key: string): SectionSpec | null {
	let best: SectionSpec | null = null;
	for (const section of ALL_SECTIONS) {
		if (key === section.key || key.startsWith(`${section.key}.`)) {
			if (!best || section.key.length > best.key.length) best = section;
		}
	}
	return best;
}

/** The value at a dotted key of a tree, or `undefined` when nothing is there. */
export function at(tree: SettingValue | undefined, key: string): SettingValue | undefined {
	let node: SettingValue | undefined = tree;
	for (const segment of key.split('.')) {
		if (node === null || typeof node !== 'object' || Array.isArray(node)) return undefined;
		node = (node as Record<string, SettingValue>)[segment];
	}
	return node;
}

export function sameValue(a: SettingValue | undefined, b: SettingValue | undefined): boolean {
	return JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
}
