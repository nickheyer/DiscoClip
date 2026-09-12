// One place every request goes through: JSON in and out, the CSRF echo a session must
// send with state changes, and errors as the API reports them.

export class ApiError extends Error {
	readonly status: number;
	/** Seconds to wait, from `Retry-After` on a 429. */
	readonly retryAfter: number | null;

	constructor(status: number, message: string, retryAfter: number | null = null) {
		super(message);
		this.name = 'ApiError';
		this.status = status;
		this.retryAfter = retryAfter;
	}
}

export function isApiError(value: unknown): value is ApiError {
	return value instanceof ApiError;
}

export function messageOf(value: unknown): string {
	if (value instanceof Error) return value.message;
	return String(value);
}

type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';

type QueryValue = string | number | boolean | null | undefined;

export interface RequestOptions {
	body?: unknown;
	query?: Record<string, QueryValue>;
	/** Statuses the caller handles itself instead of the shared 401 handling. */
	quiet?: number[];
}

let csrfToken: string | null = null;
let unauthorized: (() => void) | null = null;

export function setCsrfToken(token: string | null): void {
	csrfToken = token;
}

/** Runs when a request the session should have been good for comes back 401. */
export function onUnauthorized(handler: (() => void) | null): void {
	unauthorized = handler;
}

export function buildQuery(query: Record<string, QueryValue> | undefined): string {
	if (!query) return '';
	const params = new URLSearchParams();
	for (const [key, value] of Object.entries(query)) {
		if (value === undefined || value === null || value === '') continue;
		params.set(key, String(value));
	}
	const text = params.toString();
	return text ? `?${text}` : '';
}

async function parseBody(response: Response): Promise<unknown> {
	if (response.status === 204) return undefined;
	const type = response.headers.get('content-type') ?? '';
	const text = await response.text();
	if (!text) return undefined;
	if (type.includes('application/json')) {
		try {
			return JSON.parse(text);
		} catch {
			return text;
		}
	}
	return text;
}

function errorMessage(status: number, body: unknown): string {
	if (body && typeof body === 'object' && 'error' in body) {
		const message = (body as { error: unknown }).error;
		if (typeof message === 'string') return message;
	}
	if (typeof body === 'string' && body.trim()) return body.trim();
	switch (status) {
		case 400:
			return 'the request was malformed';
		case 401:
			return 'not logged in';
		case 403:
			return 'not allowed';
		case 404:
			return 'not found';
		case 415:
			return 'unsupported content type';
		case 422:
			return 'the request did not match what the server expects';
		case 429:
			return 'too many attempts';
		case 502:
			return 'a service the server depends on failed';
		default:
			return `the server answered ${status}`;
	}
}

export async function request<T>(
	method: Method,
	path: string,
	options: RequestOptions = {}
): Promise<T> {
	const headers: Record<string, string> = { Accept: 'application/json' };
	let body: string | undefined;
	if (options.body !== undefined) {
		headers['Content-Type'] = 'application/json';
		body = JSON.stringify(options.body);
	}
	if (method !== 'GET' && csrfToken) {
		headers['x-csrf-token'] = csrfToken;
	}
	let response: Response;
	try {
		response = await fetch(`/api${path}${buildQuery(options.query)}`, {
			method,
			headers,
			body,
			credentials: 'same-origin',
			cache: 'no-store'
		});
	} catch {
		throw new ApiError(0, 'the server could not be reached');
	}
	const parsed = await parseBody(response);
	if (response.ok) return parsed as T;
	const retryAfter = response.headers.get('retry-after');
	const error = new ApiError(
		response.status,
		errorMessage(response.status, parsed),
		retryAfter && /^\d+$/.test(retryAfter) ? Number(retryAfter) : null
	);
	if (response.status === 401 && !options.quiet?.includes(401) && unauthorized) {
		unauthorized();
	}
	throw error;
}

export const get = <T>(path: string, query?: Record<string, QueryValue>, quiet?: number[]) =>
	request<T>('GET', path, { query, quiet });
export const post = <T>(path: string, body?: unknown, quiet?: number[]) =>
	request<T>('POST', path, { body, quiet });
export const put = <T>(path: string, body?: unknown) => request<T>('PUT', path, { body });
export const patch = <T>(path: string, body?: unknown) => request<T>('PATCH', path, { body });
export const del = <T>(path: string) => request<T>('DELETE', path);

/** A GET whose answer is a text file rather than JSON. */
export async function text(path: string): Promise<string> {
	let response: Response;
	try {
		response = await fetch(`/api${path}`, {
			method: 'GET',
			headers: { Accept: 'text/plain, application/json' },
			credentials: 'same-origin',
			cache: 'no-store'
		});
	} catch {
		throw new ApiError(0, 'the server could not be reached');
	}
	const body = await response.text();
	if (response.ok) return body;
	let parsed: unknown = body;
	try {
		parsed = JSON.parse(body);
	} catch {
		// The body is the message itself.
	}
	const error = new ApiError(response.status, errorMessage(response.status, parsed));
	if (response.status === 401 && unauthorized) unauthorized();
	throw error;
}
