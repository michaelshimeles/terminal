// What the sign-in page was asked to do, read from the URL fragment, and how it hands the
// session back. Pure functions, so the rules are easy to read and check.
//
// The fragment carries only public values: the device key's public half, a random `state`
// and (desktop) a loopback port. Browsers never send the fragment to a server or in a
// Referer header.

export type Request =
	| { mode: 'desktop'; publicKey: string; state: string; port: number }
	| { mode: 'web'; publicKey: string; state: string }
	| { mode: 'export' }
	| { mode: 'invalid'; reason: string };

const PUBLIC_KEY = /^0[23][0-9a-f]{64}$/;
const STATE = /^[0-9a-f]{64}$/;

export function parseRequest(hash: string): Request {
	const p = new URLSearchParams(hash.replace(/^#/, ''));
	if (p.get('mode') === 'export') return { mode: 'export' };
	const flow = p.get('flow');
	const publicKey = (p.get('pk') ?? '').toLowerCase();
	const state = (p.get('state') ?? '').toLowerCase();
	if (flow !== 'desktop' && flow !== 'web') {
		return { mode: 'invalid', reason: 'Open this page from Terminal (Settings › Trading › Hyperliquid).' };
	}
	if (!PUBLIC_KEY.test(publicKey) || !STATE.test(state)) {
		return { mode: 'invalid', reason: 'This sign-in link is incomplete. Start again from Terminal.' };
	}
	if (flow === 'web') return { mode: 'web', publicKey, state };
	const port = Number(p.get('port'));
	if (!Number.isInteger(port) || port < 1024 || port > 65535) {
		return { mode: 'invalid', reason: 'This sign-in link is incomplete. Start again from Terminal.' };
	}
	return { mode: 'desktop', publicKey, state, port };
}

/// The desktop's loopback callback. Built only from a port number, so the page can't be used
/// to redirect anywhere else.
export function callbackUrl(port: number, state: string, answer: { session?: string; error?: string }): string {
	const q = new URLSearchParams({ state });
	if (answer.session) q.set('session', answer.session);
	if (answer.error) q.set('error', answer.error);
	return `http://127.0.0.1:${port}/callback?${q}`;
}

/// The message the web build's popup sends to its opener (same origin only).
export function webMessage(state: string, answer: { session?: string; error?: string }) {
	return { type: 'terminal-wallet', state, ...answer };
}
