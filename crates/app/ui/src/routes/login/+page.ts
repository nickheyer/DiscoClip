import type { PageLoad } from './$types';
import { providers } from '$lib/api';
import { guarded } from '$lib/api/load';

export const load: PageLoad = async () => ({
	providers: await guarded(providers.list)
});
