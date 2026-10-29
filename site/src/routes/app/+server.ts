import { redirect } from '@sveltejs/kit';
import type { RequestHandler } from './$types';

/**
 * The browser build lives in `static/app` (written by `scripts/build-web.sh`, not committed). Static
 * folders have no index page here, so send `/app` to the page itself.
 */
export const GET: RequestHandler = () => {
	redirect(308, '/app/index.html');
};
