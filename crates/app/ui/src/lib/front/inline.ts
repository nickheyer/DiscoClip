// The data a view's page carries inline, read once so the first paint asks the API for nothing

import type { FrontInline } from '$lib/api/types';

/** The id of the data block the server writes into a view's page */
export const INLINE_ID = 'discoclip-front';

/** The page's inline data for `slug`, removed from the page so a later navigation fetches afresh */
export function takeInline(slug: string): FrontInline | null {
	const element = document.getElementById(INLINE_ID);
	if (!element) return null;
	element.remove();
	const data = JSON.parse(element.textContent ?? '') as FrontInline;
	return data.slug === slug ? data : null;
}
