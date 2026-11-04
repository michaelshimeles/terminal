// The sign-in page runs only in the browser: the Turnkey SDK uses WebCrypto and IndexedDB,
// and everything it needs is in the URL fragment.
export const prerender = true;
export const ssr = false;
