import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';
import { front, isApiError } from '$lib/api';
import type { FrontJobQuery, MediaKind } from '$lib/api';

const KINDS: MediaKind[] = ['video', 'audio', 'image', 'file'];

export const load: PageLoad = async ({ params, url, depends }) => {
	depends(`front:${params.slug}:jobs`);
	const media = url.searchParams.get('media');
	const query: FrontJobQuery = {
		q: url.searchParams.get('q') ?? undefined,
		media: media && KINDS.includes(media as MediaKind) ? (media as MediaKind) : undefined,
		resolver: url.searchParams.get('platform') ?? undefined
	};
	try {
		const page = await front.jobs(params.slug, query);
		return { query, page };
	} catch (cause) {
		if (isApiError(cause) && cause.status === 401) {
			redirect(307, `/f/${params.slug}/login?next=${encodeURIComponent(`${url.pathname}${url.search}`)}`);
		}
		throw cause;
	}
};
