import { error } from '@sveltejs/kit';
import type { LayoutLoad } from './$types';
import { front, isApiError } from '$lib/api';
// Refresh viewer identity on each page to reflect login changes.
export const load: LayoutLoad = async ({ params, depends }) => {
	depends(`front:${params.slug}`);
	try {
		const info = await front.info(params.slug);
		return { slug: params.slug, info };
	} catch (cause) {
		if (isApiError(cause)) {
			if (cause.status === 404) error(404, 'There is no such site here.');
			error(cause.status === 0 ? 503 : cause.status, cause.message);
		}
		throw cause;
	}
};
