<script lang="ts">
	// Terminal's wallet sign-in: an email code or a passkey, through Turnkey. When the user is
	// in, the page asks Turnkey for a 15-minute session bound to the device's key, hands it
	// back (desktop: a redirect to 127.0.0.1; web: a message to the opener) and logs out. In
	// export mode it shows the recovery phrase in Turnkey's own iframe instead.
	import { onMount, tick } from 'svelte';
	import type { TurnkeyClient } from '@turnkey/core';
	import { callbackUrl, parseRequest, webMessage, type Request } from '#lib/wallet/handoff.js';
	import * as tk from '#lib/wallet/turnkey.js';

	type Stage = 'start' | 'code' | 'working' | 'handed' | 'export' | 'done';

	let request: Request = $state({ mode: 'invalid', reason: '' });
	let stage: Stage = $state('start');
	let email = $state('');
	let code = $state('');
	let error = $state('');
	let busy = $state(false);
	let otp: { otpId: string; otpEncryptionTargetBundle: string } | null = null;
	let client: TurnkeyClient | null = null;
	let frame: HTMLElement | undefined = $state();

	onMount(() => {
		request = parseRequest(window.location.hash);
		// The fragment has done its job; keep it out of history.
		history.replaceState(null, '', window.location.pathname);
	});

	async function turnkey(): Promise<TurnkeyClient> {
		const c = client ?? (await tk.client());
		client = c;
		return c;
	}

	async function run(f: () => Promise<void>) {
		error = '';
		busy = true;
		try {
			await f();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			busy = false;
		}
	}

	const sendCode = () =>
		run(async () => {
			otp = await tk.sendCode(await turnkey(), email.trim());
			stage = 'code';
		});

	const verify = () =>
		run(async () => {
			if (!otp) return;
			const c = await turnkey();
			// Backing up opens an existing wallet; it never creates one.
			await (request.mode === 'export' ? tk.verifyCodeLoginOnly(c, otp, code) : tk.verifyCode(c, email.trim(), otp, code));
			await signedIn();
		});

	const passkey = (create: boolean) =>
		run(async () => {
			const c = await turnkey();
			await (create ? tk.passkeySignUp(c) : tk.passkeyLogin(c));
			await signedIn();
		});

	async function signedIn() {
		const c = await turnkey();
		if (request.mode === 'export') {
			stage = 'export';
			await tick();
			if (frame) await tk.showRecoveryPhrase(c, frame);
			return;
		}
		if (request.mode !== 'desktop' && request.mode !== 'web') return;
		stage = 'working';
		let session: string;
		try {
			session = await tk.sessionForDevice(c, request.publicKey);
		} catch (e) {
			// The page logged itself out either way: back to the sign-in controls (Cancel included).
			stage = 'start';
			otp = null;
			code = '';
			const why = e instanceof Error ? e.message : String(e);
			throw new Error(`Signed in, but Turnkey didn't connect this device (${why}). Sign in again, or cancel.`);
		}
		answer({ session });
	}

	/// Hand the answer back to Terminal.
	function answer(a: { session?: string; error?: string }) {
		if (request.mode === 'desktop') {
			stage = 'handed';
			window.location.replace(callbackUrl(request.port, request.state, a));
		} else if (request.mode === 'web') {
			stage = 'handed';
			window.opener?.postMessage(webMessage(request.state, a), window.location.origin);
			window.close();
		}
	}

	const cancel = () => answer({ error: 'Sign-in cancelled.' });

	const finishExport = () =>
		run(async () => {
			if (client) await tk.signOut(client);
			frame?.replaceChildren();
			stage = 'done';
		});
</script>

<svelte:head>
	<title>Terminal wallet</title>
	<meta name="referrer" content="no-referrer" />
	<meta name="robots" content="noindex" />
</svelte:head>

<main>
	<section class="card">
		<header>
			<h1>Terminal wallet</h1>
			<span class="testnet">TESTNET</span>
		</header>

		{#if request.mode === 'invalid'}
			<p class="muted">{request.reason || 'Loading…'}</p>
		{:else if !tk.configured()}
			<p>Wallet sign-in isn't set up on this site yet.</p>
			<p class="muted">Terminal's Turnkey organization hasn't been configured, so no wallet can be created or opened here.</p>
			{#if request.mode !== 'export'}
				<button class="secondary" onclick={cancel}>Back to Terminal</button>
			{/if}
		{:else if stage === 'handed'}
			<p>Signed in. Go back to Terminal; you can close this tab.</p>
		{:else if stage === 'working'}
			<p class="muted">Connecting your wallet to Terminal…</p>
		{:else if stage === 'export'}
			<p class="warn">Your recovery phrase controls the wallet. Write it down offline and never share it.</p>
			<div class="frame" bind:this={frame}></div>
			<button onclick={finishExport} disabled={busy}>Done, sign out</button>
		{:else if stage === 'done'}
			<p>Signed out. You can close this tab.</p>
		{:else}
			<p class="muted">
				{#if request.mode === 'export'}
					Sign in again to back up your wallet. Turnkey shows the recovery phrase in its own secure frame on this page.
					Terminal never sees it.
				{:else}
					Sign in, or create your wallet. Terminal then gets a 15-minute session for this device only. Your wallet is
					held by Turnkey; Terminal never sees its key.
				{/if}
			</p>

			{#if stage === 'start'}
				<form onsubmit={(e) => (e.preventDefault(), sendCode())}>
					<label>Email<input type="email" autocomplete="email" required bind:value={email} disabled={busy} /></label>
					<button type="submit" disabled={busy || !email.includes('@')}>Email me a code</button>
				</form>
				<div class="or">or</div>
				<div class="row" class:single={request.mode === 'export'}>
					<button class="secondary" onclick={() => passkey(false)} disabled={busy}>Sign in with a passkey</button>
					{#if request.mode !== 'export'}
						<button class="secondary" onclick={() => passkey(true)} disabled={busy}>Create with a passkey</button>
					{/if}
				</div>
			{:else if stage === 'code'}
				<form onsubmit={(e) => (e.preventDefault(), verify())}>
					<label
						>Code sent to {email}<input
							inputmode="text"
							autocomplete="one-time-code"
							required
							bind:value={code}
							disabled={busy}
						/></label
					>
					<button type="submit" disabled={busy || code.trim().length < 6}>Continue</button>
				</form>
				<button class="link" onclick={() => ((stage = 'start'), (code = ''))} disabled={busy}>Use another email</button>
			{/if}

			{#if request.mode !== 'export'}
				<button class="link" onclick={cancel} disabled={busy}>Cancel and go back to Terminal</button>
			{/if}
		{/if}

		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}

		<footer class="muted">Hyperliquid testnet only. Terminal can't reach mainnet.</footer>
	</section>
</main>

<style>
	main {
		min-height: 100vh;
		display: grid;
		place-items: center;
		padding: 24px 16px;
		background: var(--bg);
		color: var(--fg);
		font-family: var(--sans);
	}
	.card {
		width: 100%;
		max-width: 420px;
		background: var(--card);
		box-shadow: var(--card-edge);
		border-radius: 14px;
		padding: 24px;
		display: grid;
		gap: 14px;
	}
	header {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	h1 {
		font: 600 20px var(--display);
		margin: 0;
	}
	.testnet {
		background: #f5a623;
		color: #000;
		border-radius: 9px;
		padding: 2px 8px;
		font: 700 11px var(--mono);
	}
	p {
		margin: 0;
		line-height: 1.45;
	}
	.muted {
		color: var(--muted);
		font-size: 14px;
	}
	.warn {
		color: #f5a623;
		font-size: 14px;
	}
	.error {
		color: #ff6b5f;
		font-size: 14px;
	}
	form {
		display: grid;
		gap: 10px;
	}
	label {
		display: grid;
		gap: 6px;
		font-size: 14px;
	}
	input {
		font: 15px var(--sans);
		padding: 10px 12px;
		border-radius: 8px;
		border: 1px solid var(--line);
		background: var(--bg);
		color: var(--fg);
	}
	button {
		font: 600 14px var(--sans);
		padding: 10px 14px;
		border-radius: 8px;
		border: 0;
		background: var(--solid);
		color: var(--on-solid);
		cursor: pointer;
	}
	button:disabled {
		opacity: 0.5;
		cursor: default;
	}
	button.secondary {
		background: transparent;
		color: var(--fg);
		box-shadow: inset 0 0 0 1px var(--line);
	}
	button.link {
		background: none;
		color: var(--muted);
		padding: 0;
		font-weight: 400;
		justify-self: start;
	}
	.row {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 8px;
	}
	.row.single {
		grid-template-columns: 1fr;
	}
	@media (max-width: 420px) {
		.row {
			grid-template-columns: 1fr;
		}
	}
	.or {
		text-align: center;
		color: var(--muted);
		font-size: 13px;
	}
	.frame {
		min-height: 180px;
	}
	.frame :global(iframe) {
		width: 100%;
		min-height: 180px;
		border: 0;
		border-radius: 8px;
		background: #fff;
	}
	footer {
		font-size: 12px;
		border-top: 1px solid var(--line);
		padding-top: 12px;
	}
</style>
