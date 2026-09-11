import type { CallbackError } from './api/types';

/** What each `?error=` a provider flow can end in means to the person reading it. */
export const CALLBACK_ERRORS: Record<CallbackError, string> = {
	state: 'That login attempt expired or was already used. Start again.',
	denied: 'You cancelled at the provider, so nothing was changed.',
	provider: 'The provider refused the request.',
	identity: 'The provider did not send back a usable identity.',
	exchange: 'The provider could not be reached to finish the login.',
	session: 'Your session changed while linking. Log in and try again.',
	already_linked: 'That identity is already linked to another account.',
	provider_linked:
		'This account is already linked to a different identity at that provider. Unlink it first.',
	unknown_identity:
		'No account is linked to that identity. Log in with your password, then link it from your account page.'
};

export function callbackErrorMessage(code: string | null): string | null {
	if (!code) return null;
	if (code in CALLBACK_ERRORS) return CALLBACK_ERRORS[code as CallbackError];
	return `The provider flow failed (${code}).`;
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
