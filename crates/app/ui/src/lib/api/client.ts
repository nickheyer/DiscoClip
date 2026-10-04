// The one place the app talks HTTP to the backend.

export type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';

/** Query values. `undefined` and `null` entries are left out of the URL. */
export type Query = Record<string, string | number | boolean | null | undefined>;

export interface RequestOptions {
	/** A `401` answers a question rather than ending the session, as on a login form. */
	expectUnauthorized?: boolean;
}

export class ApiError extends Error {
	readonly status: number;
	/** Seconds to wait before trying again, from `Retry-After`. */
	readonly retryAfter: number | null;

	constructor(status: number, message: string, retryAfter: number | null = null) {
		super(message);
		this.name = 'ApiError';
		this.status = status;
		this.retryAfter = retryAfter;
	}

	get unauthorized(): boolean {
		return this.status === 401;
	}

	get rateLimited(): boolean {
		return this.status === 429;
	}
}

const PREFIX = '/api';

let csrfToken: string | null = null;

/** The token every mutating call echoes in `x-csrf-token`. The session store keeps it current. */
export function setCsrfToken(token: string | null): void {
	csrfToken = token;
}

type UnauthorizedHandler = () => void;
const unauthorizedHandlers = new Set<UnauthorizedHandler>();

/** Runs `handler` whenever a request the caller did not expect to fail answers `401`. */
export function onUnauthorized(handler: UnauthorizedHandler): () => void {
	unauthorizedHandlers.add(handler);
	return () => {
		unauthorizedHandlers.delete(handler);
	};
}

function encode(query?: Query): string {
	if (!query) return '';
	const params = new URLSearchParams();
	for (const [key, value] of Object.entries(query)) {
		if (value === undefined || value === null) continue;
		params.set(key, String(value));
	}
	const text = params.toString();
	return text ? `?${text}` : '';
}

/** A URL under `/api` for links the browser follows itself: downloads, exports, streams. */
export function fileUrl(path: string, query?: Query): string {
	return `${PREFIX}${path}${encode(query)}`;
}

function retryAfterOf(response: Response): number | null {
	const header = response.headers.get('Retry-After');
	if (!header) return null;
	const seconds = Number(header);
	if (Number.isFinite(seconds)) return Math.max(0, Math.ceil(seconds));
	const at = Date.parse(header);
	return Number.isNaN(at) ? null : Math.max(0, Math.ceil((at - Date.now()) / 1000));
}

async function messageOf(response: Response): Promise<string> {
	const type = response.headers.get('Content-Type') ?? '';
	const text = await response.text();
	if (type.includes('application/json')) {
		try {
			const body = JSON.parse(text) as { error?: unknown };
			if (typeof body.error === 'string' && body.error) return body.error;
		} catch {
			return text || response.statusText;
		}
	}
	return text.trim() || response.statusText || `HTTP ${response.status}`;
}

/**
 * Sends one request and returns the decoded JSON body. A `204` returns `undefined`.
 * Every failure, including an unreachable server, throws an `ApiError`.
 */
export async function request<T>(
	method: Method,
	path: string,
	body?: unknown,
	query?: Query,
	options: RequestOptions = {}
): Promise<T> {
	const headers = new Headers({ Accept: 'application/json' });
	const init: RequestInit = { method, headers, credentials: 'same-origin' };
	if (body !== undefined) {
		headers.set('Content-Type', 'application/json');
		init.body = JSON.stringify(body);
	}
	if (method !== 'GET' && csrfToken) {
		headers.set('x-csrf-token', csrfToken);
	}

	let response: Response;
	try {
		response = await fetch(`${PREFIX}${path}${encode(query)}`, init);
	} catch {
		throw new ApiError(0, 'The server could not be reached.');
	}

	if (!response.ok) {
		const error = new ApiError(response.status, await messageOf(response), retryAfterOf(response));
		if (error.unauthorized && !options.expectUnauthorized) {
			for (const handler of unauthorizedHandlers) handler();
		}
		throw error;
	}
	if (response.status === 204) {
		return undefined as T;
	}
	return (await response.json()) as T;
}

export const get = <T>(path: string, query?: Query, options?: RequestOptions) =>
	request<T>('GET', path, undefined, query, options);
export const post = <T>(path: string, body?: unknown, query?: Query, options?: RequestOptions) =>
	request<T>('POST', path, body, query, options);
export const put = <T>(path: string, body?: unknown, query?: Query, options?: RequestOptions) =>
	request<T>('PUT', path, body, query, options);
export const patch = <T>(path: string, body?: unknown, query?: Query, options?: RequestOptions) =>
	request<T>('PATCH', path, body, query, options);
export const del = <T>(path: string, query?: Query, options?: RequestOptions) =>
	request<T>('DELETE', path, undefined, query, options);
