// Editable settings. Dotted keys match the backend Settings type.

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
	/** The dotted key of the section. Fields live under it. */
	key: string;
	title: string;
	description: string;
	icon: string;
	fields: FieldSpec[];
	/** Sections beneath this one. */
	sections?: SectionSpec[];
	/** Maps beneath this one. */
	maps?: MapSpec[];
	/** Optional sections are either null or complete objects. */
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
		description: 'Log level and filters.',
		icon: 'file-text',
		fields: [
			{
				name: 'level',
				label: 'Filter',
				kind: 'text',
				hint: 'Use a level such as info, or a filter such as info,discoclip_engine=debug.',
				placeholder: 'info'
			}
		]
	},
	{
		key: 'engine',
		title: 'Engine',
		description: 'Storage and job processing.',
		icon: 'zap',
		fields: [
			{
				name: 'cache_dir',
				label: 'Cache directory',
				kind: 'path',
				hint: 'Stores downloads, outputs and ffmpeg. A new path applies to new jobs.'
			},
			{
				name: 'workers',
				label: 'Workers',
				kind: 'integer',
				hint: 'Number of concurrent jobs.',
				min: 1
			}
		],
		sections: [
			{
				key: 'engine.limits',
				title: 'Limits',
				description: 'Maximum limits for all jobs.',
				icon: 'shield',
				fields: [
					{
						name: 'max_source_bytes',
						label: 'Maximum download size',
						kind: 'bytes',
						hint: 'The biggest download accepted.',
						min: 1
					},
					{
						name: 'max_duration_secs',
						label: 'Maximum video duration',
						kind: 'seconds',
						hint: 'Leave blank for no limit. Zero rejects all videos, including live streams.',
						nullable: true,
						nullLabel: 'No limit',
						min: 0
					},
					{
						name: 'max_height',
						label: 'Maximum output height',
						kind: 'integer',
						hint: 'Larger videos are resized to this height in pixels.',
						min: 1
					}
				]
			},
			{
				key: 'engine.archive',
				title: 'Archive',
				description: 'Save completed clips outside the cache.',
				icon: 'download',
				optional: {
					label: 'Archive finished videos',
					hint: 'Retain a separate copy of each completed clip.'
				},
				fields: [
					{
						name: 'dir',
						label: 'Archive directory',
						kind: 'path',
						hint: 'Files are grouped by year and month.',
						placeholder: 'archive'
					},
					{
						name: 'keep',
						label: 'Keep',
						kind: 'enum',
						hint: 'Choose the files to archive.',
						options: [
							{ value: 'output', label: 'Processed file' },
							{ value: 'source', label: 'Original file' },
							{ value: 'both', label: 'Both' }
						]
					}
				]
			},
			{
				key: 'engine.playlists',
				title: 'Playlists',
				description: 'Create one job for each playlist entry.',
				icon: 'rules',
				fields: [
					{
						name: 'enabled',
						label: 'Expand playlists',
						kind: 'boolean',
						hint: 'Accept playlist links.'
					},
					{
						name: 'max_entries',
						label: 'Maximum entries',
						kind: 'integer',
						hint: 'Maximum jobs per playlist.',
						min: 1
					}
				]
			},
			{
				key: 'engine.live',
				title: 'Live streams',
				description: 'Record streams from the time the link is submitted.',
				icon: 'activity',
				fields: [
					{
						name: 'max_capture_secs',
						label: 'Maximum recording duration',
						kind: 'seconds',
						hint: 'Stop recording after this duration.',
						min: 0
					}
				]
			},
			{
				key: 'engine.download',
				title: 'Downloads',
				description:
					'Parallel connections and recovery for HTTP downloads.',
				icon: 'download',
				fields: [
					{
						name: 'connections',
						label: 'Connections per file',
						kind: 'integer',
						hint: 'Used when the source supports partial downloads.',
						min: 1
					},
					{
						name: 'chunk_bytes',
						label: 'Chunk size',
						kind: 'bytes',
						hint: 'Bytes per request. Minimum 64 KiB.',
						min: 65536
					},
					{
						name: 'resume_attempts',
						label: 'Retry attempts',
						kind: 'integer',
						hint: 'Retries after an interrupted download. Zero disables retries.',
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
						hint: 'Delete completed jobs after this many days. Zero keeps them.',
						min: 0
					},
					{
						name: 'failed_jobs_days',
						label: 'Failed and cancelled jobs',
						kind: 'days',
						hint: 'Delete failed and cancelled jobs after this many days. Zero keeps them.',
						min: 0
					},
					{
						name: 'cache_max_bytes',
						label: 'Cache size',
						kind: 'bytes',
						hint: 'Delete the oldest cached files above this limit. Zero disables cleanup.',
						min: 0
					},
					{
						name: 'sweep_interval_secs',
						label: 'Cleanup interval',
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
		description: 'Outgoing connections to media platforms.',
		icon: 'globe',
		fields: [
			{
				name: 'user_agent',
				label: 'User agent',
				kind: 'text',
				hint: 'Default user agent for outgoing requests.'
			},
			{
				name: 'connect_timeout_secs',
				label: 'Connect timeout',
				kind: 'seconds',
				hint: 'Time allowed to connect to a host.',
				min: 1
			},
			{
				name: 'request_timeout_secs',
				label: 'Request timeout',
				kind: 'seconds',
				hint: 'Time limit for page and API requests. Excludes media downloads.',
				min: 1
			},
			{
				name: 'read_timeout_secs',
				label: 'Read timeout',
				kind: 'seconds',
				hint: 'Maximum delay between two chunks of a body.',
				min: 1
			},
			{
				name: 'max_redirects',
				label: 'Redirects followed',
				kind: 'integer',
				hint: 'Maximum redirects per request.',
				min: 0
			}
		],
		sections: [
			{
				key: 'http.retry',
				title: 'Retries',
				description: 'Retry failed requests with increasing delays.',
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
						label: 'Initial delay',
						kind: 'millis',
						hint: 'Delay before the first retry. Doubles with each retry.',
						min: 0
					},
					{
						name: 'max_ms',
						label: 'Maximum delay',
						kind: 'millis',
						hint: 'Maximum delay between retries.',
						min: 0
					}
				]
			},
			{
				key: 'http.rate_limits',
				title: 'Rate limits',
				description: 'Limit requests to each host.',
				icon: 'clock',
				fields: [],
				sections: [
					{
						key: 'http.rate_limits.default',
						title: 'Default rate',
						description: 'Applies to hosts without a custom rate.',
						icon: 'clock',
						fields: [
							{
								name: 'per_second',
								label: 'Requests per second',
								kind: 'number',
								hint: 'Average request rate. Zero disables the limit.',
								min: 0
							},
							{
								name: 'burst',
								label: 'Burst',
								kind: 'integer',
								hint: 'Maximum requests in a burst.',
								min: 0
							}
						]
					}
				],
				maps: [
					{
						key: 'http.rate_limits.hosts',
						label: 'Per host',
						hint: 'Host rules include subdomains. The most specific match applies.',
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
				description: 'HTTP and SOCKS proxies for outgoing requests.',
				icon: 'server',
				fields: [
					{
						name: 'default',
						label: 'Default proxy',
						kind: 'url',
						hint: 'HTTP or SOCKS proxy URL. Leave blank for a direct connection.',
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
						hint: 'Use the platform ID, such as tiktok or youtube.',
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
						hint: 'Includes subdomains. The most specific match applies.',
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
		description: 'Output files from web submissions.',
		icon: 'download',
		fields: [
			{
				name: 'dir',
				label: 'Directory',
				kind: 'path',
				hint: 'Stores clips submitted through the web app.'
			},
			{
				name: 'max_bytes',
				label: 'Maximum output size',
				kind: 'bytes',
				hint: 'Maximum file size for web submissions.',
				min: 1
			}
		]
	},
	{
		key: 'fixtures',
		title: 'Platform checks',
		description:
			'Scheduled checks of supported platforms.',
		icon: 'check-circle',
		fields: [
			{
				name: 'interval_secs',
				label: 'Check interval',
				kind: 'seconds',
				hint: 'Zero disables scheduled checks.',
				min: 0
			},
			{
				name: 'timeout_secs',
				label: 'Link timeout',
				kind: 'seconds',
				hint: 'Time limit for each test link.',
				min: 1
			}
		]
	},
	{
		key: 'web',
		title: 'Web app',
		description: 'Address, HTTPS and reverse proxies.',
		icon: 'monitor',
		fields: [
			{
				name: 'bind',
				label: 'Listen on',
				kind: 'socket',
				hint: 'IP address and port, such as 127.0.0.1:8080. Changes apply immediately.',
				placeholder: '127.0.0.1:8080'
			},
			{
				name: 'public_url',
				label: 'Public URL',
				kind: 'url',
				hint: 'Public address used for login redirects and shared links. Leave blank to use the request address.',
				nullable: true,
				nullLabel: 'From each request',
				placeholder: 'https://clips.example.com'
			},
			{
				name: 'trusted_proxies',
				label: 'Trusted proxies',
				kind: 'list',
				hint: 'IP addresses or CIDR networks allowed to set forwarding headers.',
				validate: networkProblem,
				placeholder: '10.0.0.0/8'
			}
		],
		sections: [
			{
				key: 'web.tls',
				title: 'HTTPS',
				description: 'Certificates reload automatically when their files change.',
				icon: 'lock',
				optional: {
					label: 'Serve HTTPS',
					hint: 'Turn off when a reverse proxy handles HTTPS.'
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
		description: 'Configure external login providers. Discord login is configured under Applications.',
		icon: 'key',
		fields: [
			{
				name: 'oauth_signup',
				label: 'Allow account registration',
				kind: 'boolean',
				hint: 'Create a viewer account at the first login.'
			}
		],
		sections: [
			{
				key: 'auth.github',
				title: 'GitHub',
				description: 'An OAuth app registered at GitHub.',
				icon: 'github',
				optional: { label: 'Enable GitHub login', hint: '' },
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
				optional: { label: 'Enable Google login', hint: '' },
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
				optional: { label: 'Enable single sign-on', hint: '' },
				fields: [
					{
						name: 'name',
						label: 'Button label',
						kind: 'text',
						hint: 'Name shown on the login button.',
						placeholder: 'Single sign-on'
					},
					{
						name: 'issuer',
						label: 'Issuer',
						kind: 'url',
						hint: 'Base URL used for OpenID Connect discovery.',
						placeholder: 'https://login.example.com/realms/main'
					},
					{ name: 'client_id', label: 'Client ID', kind: 'text', hint: '' },
					{ name: 'client_secret', label: 'Client secret', kind: 'secret', hint: '' },
					{
						name: 'scopes',
						label: 'Scopes',
						kind: 'list',
						hint: 'Include openid.',
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
