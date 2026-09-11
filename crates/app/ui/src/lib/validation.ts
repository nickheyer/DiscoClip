// The server's own limits, checked before a request leaves the browser.

export const USERNAME_MAX = 32;
export const USERNAME_RE = /^[A-Za-z0-9._-]{1,32}$/;
export const PASSWORD_MIN = 8;
export const PASSWORD_MAX = 256;
export const TOKEN_NAME_MAX = 64;
export const APPLICATION_NAME_MAX = 100;

export function usernameProblem(value: string): string | null {
	if (value === '') return null;
	return USERNAME_RE.test(value)
		? null
		: `A username is 1 to ${USERNAME_MAX} letters, digits, '.', '_' or '-'.`;
}

export function passwordProblem(value: string): string | null {
	if (value === '') return null;
	return value.length >= PASSWORD_MIN && value.length <= PASSWORD_MAX
		? null
		: `A password is ${PASSWORD_MIN} to ${PASSWORD_MAX} characters.`;
}

export function matchProblem(password: string, confirmation: string): string | null {
	if (confirmation === '') return null;
	return password === confirmation ? null : 'The passwords differ.';
}
