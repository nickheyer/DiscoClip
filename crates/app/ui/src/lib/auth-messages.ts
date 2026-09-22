import type { CallbackError } from './api/types';

/** Login error messages. */
export const CALLBACK_ERRORS: Record<CallbackError, string> = {
	state: 'Login expired. Try again.',
	denied: 'Login cancelled.',
	provider: 'The provider refused the request.',
	identity: 'Could not identify your account. Try again.',
	exchange: 'Could not reach the login provider. Try again.',
	session: 'Your session changed while linking. Log in and try again.',
	already_linked: 'That identity is already linked to another account.',
	provider_linked:
		'Unlink the current provider account before linking another.',
	unknown_identity:
		'Log in with your password, then link this provider in Account.'
};

export function callbackErrorMessage(code: string | null): string | null {
	if (!code) return null;
	if (code in CALLBACK_ERRORS) return CALLBACK_ERRORS[code as CallbackError];
	return `Login failed (${code}).`;
}

export function providerIcon(id: string): string {
	switch (id) {
		case 'discord':
		case 'github':
		case 'google':
			return id;
		default:
			return 'key';
	}
}
