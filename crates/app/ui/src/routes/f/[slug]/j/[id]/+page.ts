import { error, redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';
import { front, isApiError } from '$lib/api';

export const load: PageLoad = async ({ params, url }) => {
	try {
		const job = await front.job(params.slug, params.id);
		return { job };
	} catch (cause) {
		if (isApiError(cause)) {
			if (cause.status === 401) {
				redirect(307, `/f/${params.slug}/login?next=${encodeURIComponent(`${url.pathname}${url.search}`)}`);
			}
			if (cause.status === 404) error(404, 'This site has no such media.');
			error(cause.status === 0 ? 503 : cause.status, cause.message);
		}
		throw cause;
	}
};
