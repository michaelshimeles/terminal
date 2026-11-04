// Turnkey on the sign-in page, through `@turnkey/core` and Turnkey's Auth Proxy. The proxy
// creates each user's sub-organization with a key Turnkey holds, so no Terminal server has a
// Turnkey credential. The IDs below are public identifiers, not secrets.

import { OtpType, TurnkeyClient } from '@turnkey/core';
import { IframeStamper } from '@turnkey/iframe-stamper';

const env = import.meta.env;
export const ORGANIZATION_ID: string = env.VITE_TURNKEY_ORGANIZATION_ID ?? '';
export const AUTH_PROXY_CONFIG_ID: string = env.VITE_TURNKEY_AUTH_PROXY_CONFIG_ID ?? '';
const API_BASE_URL: string | undefined = env.VITE_TURNKEY_API_BASE_URL || undefined;
const AUTH_PROXY_URL: string | undefined = env.VITE_TURNKEY_AUTH_PROXY_URL || undefined;
const EXPORT_IFRAME_URL = 'https://export.turnkey.com';

/// How long the device's session lasts (Turnkey's default). Enough to approve a trading key.
export const SESSION_SECONDS = '900';

/// The wallet a new user gets: one Ethereum account, the user's Hyperliquid account.
const WALLET = {
	walletName: 'Terminal',
	walletAccounts: [
		{
			curve: 'CURVE_SECP256K1' as const,
			pathFormat: 'PATH_FORMAT_BIP32' as const,
			path: "m/44'/60'/0'/0/0",
			addressFormat: 'ADDRESS_FORMAT_ETHEREUM' as const
		}
	]
};

export const configured = () => ORGANIZATION_ID !== '' && AUTH_PROXY_CONFIG_ID !== '';

export async function client(): Promise<TurnkeyClient> {
	const c = new TurnkeyClient({
		organizationId: ORGANIZATION_ID,
		authProxyConfigId: AUTH_PROXY_CONFIG_ID,
		...(API_BASE_URL ? { apiBaseUrl: API_BASE_URL } : {}),
		...(AUTH_PROXY_URL ? { authProxyUrl: AUTH_PROXY_URL } : {})
	});
	await c.init();
	return c;
}

export async function sendCode(c: TurnkeyClient, email: string) {
	return c.initOtp({ otpType: OtpType.Email, contact: email });
}

/// Verify the emailed code: logs in, or signs up with a new wallet.
export async function verifyCode(
	c: TurnkeyClient,
	email: string,
	otp: { otpId: string; otpEncryptionTargetBundle: string },
	code: string
) {
	return c.completeOtp({
		otpId: otp.otpId,
		otpEncryptionTargetBundle: otp.otpEncryptionTargetBundle,
		otpCode: code.trim(),
		contact: email,
		otpType: OtpType.Email,
		createSubOrgParams: { customWallet: WALLET }
	});
}

export async function passkeyLogin(c: TurnkeyClient) {
	return c.loginWithPasskey();
}

export async function passkeySignUp(c: TurnkeyClient) {
	return c.signUpWithPasskey({ passkeyDisplayName: 'Terminal', createSubOrgParams: { customWallet: WALLET } });
}

/// Ask Turnkey for a session bound to the device's key (Turnkey's own `stamp_login`, which the
/// SDK's `refreshSession` uses too), then log this page out so the browser keeps nothing.
export async function sessionForDevice(c: TurnkeyClient, devicePublicKey: string): Promise<string> {
	const mine = await c.getSession();
	if (!mine) throw new Error('Not signed in.');
	try {
		const r = await c.httpClient.stampLogin({
			publicKey: devicePublicKey,
			organizationId: mine.organizationId,
			expirationSeconds: SESSION_SECONDS
		});
		if (!r?.session) throw new Error('Turnkey returned no session.');
		return r.session;
	} finally {
		await c.logout().catch(() => {});
	}
}

/// Show the wallet's recovery phrase inside Turnkey's export iframe. The phrase is decrypted
/// only in that iframe (another origin): never in this page's code, never on a server.
export async function showRecoveryPhrase(c: TurnkeyClient, container: HTMLElement): Promise<void> {
	const session = await c.getSession();
	if (!session) throw new Error('Not signed in.');
	const frame = new IframeStamper({
		iframeUrl: EXPORT_IFRAME_URL,
		iframeElementId: 'turnkey-export',
		iframeContainer: container
	});
	const target = await frame.init();
	const wallets = await c.fetchWallets();
	const wallet = wallets[0];
	if (!wallet) throw new Error('This account has no wallet.');
	const bundle = await c.exportWallet({ walletId: wallet.walletId, targetPublicKey: target });
	const shown = await frame.injectWalletExportBundle(bundle, session.organizationId);
	if (!shown) throw new Error("Turnkey's frame couldn't show the phrase.");
}

export async function signOut(c: TurnkeyClient) {
	await c.logout().catch(() => {});
}
