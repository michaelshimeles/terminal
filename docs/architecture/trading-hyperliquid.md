# Trading on Hyperliquid: design

Status: **v1 built for testnet** (decisions in §15, as built in §16). Mainnet is not reachable from this build; it waits on §12.
Author: Grok Bot, for Micky; v1 by Claude. Research date: 2026-11-02 (read-only calls about 23:30 UTC). Times are UTC unless marked.

Micky asked: *"i wanna enable crypto trading and stocks (via hyperliquid)"*. This doc designs order entry on Hyperliquid from Terminal's desktop app and the WASM web build. That covers crypto (core perps such as `BTC`, and spot such as `HYPE/USDC`) and the HIP-3 equity perps the terminal already shows (`xyz:AAPL`, labelled `PERP · HL xyz`). It fits the order entry that exists today: the demo account, the SnapTrade opt-in, the `PORT`/`POS`/`ORD`/`ACTV` panes and the order ticket.

**Read §12 (legal) first.** Hyperliquid's Terms of Use name Ontario, Canada as a restricted region. This needs a decision from Micky before any phase touches mainnet, and it affects how testnet can be funded too. Micky's answer for now (§15): build and demo on testnet; legal review before mainnet.

Evidence tags used below:

- **[doc: X]** is from Hyperliquid's docs, Terms of Use or SDK source (references at the end).
- **[observed]** is from a read-only call or a local build I ran on 2026-11-02. These are snapshots, not guarantees.
- **[repo]** is from reading `michaelshimeles/terminal` at `0690de8`.
- **[inferred]** and **[unverified]** are my reasoning, to be confirmed on testnet in phase 1.

---

## 1. Goals and non-goals

### Goals

1. Place, modify and cancel orders on Hyperliquid from the order ticket: core perps, spot, and HIP-3 perps on the `xyz`, `para`, `io` and `mkts` dexes.
2. Show the account live: positions, open orders, fills, funding, margin and PnL, in the existing `PORT`/`POS`/`ORD`/`ACTV` panes, from Hyperliquid's user websocket streams.
3. Keep custody with the user. Terminal never sees the user's main wallet key, and Terminal's servers never sign anything.
4. Ship the same flow on desktop (macOS first) and on the web build at `terminal.rasmic.xyz/app`.
5. Be safe by default: off until the user opts in, testnet first, a confirmation on every order, a max-notional guard, a kill switch, and no keys in logs.
6. Never present a HIP-3 equity perp as a share.

### Non-goals (for this design)

- **No server-side signing or order routing.** `tr-gateway` stays data-only, as `docs/architecture/server.md` §1 already decided ("It never signs, routes or places orders") [repo]. §3 explains why this doc keeps that.
- No deposits, withdrawals or transfers in v1. Users fund the account on Hyperliquid themselves. (`usdClassTransfer` between spot and perp may come later, §11.)
- No vaults, sub-accounts, multi-sig, staking, TWAP or outcome markets in v1.
- No automated or algorithmic trading. Every order starts from a human click or command.
- No change to the demo account or to SnapTrade.

---

## 2. How Terminal trades today (what this fits into)

| Piece | Today | Where |
|---|---|---|
| Account modes | `AccountMode::{Demo, Live}`. Demo is the default: one-click orders filled locally against the terminal's quotes. Live means SnapTrade. The toolbar shows a `DEMO`/`LIVE` badge. | `app/src/prefs.rs`, `app/src/app/chrome.rs` [repo] |
| Opt-in | `TradingPrefs { account_mode, enabled, max_order_value, default_account }`. Live orders stay off until Settings › Trading is enabled, behind an acknowledgement dialog ("I understand that orders are real…"). | `app/src/prefs.rs`, `app/src/app/trading.rs` `enable_dialog` [repo] |
| Order ticket | `Ticket` (symbol, side, `OrderType::{Market, Limit, Stop, StopLimit}`, `TimeInForce::{Day, Gtc, Fok, Ioc}`, units or dollars, `SELL ALL`). It opens from `BUY`/`SELL`/`TRADE`, ⇧B/⇧S, chart, watchlist and position rows, and the ⌘K palette. | `app/src/app/trading.rs` [repo] |
| Live safety rails | (1) off until enabled; (2) an order can only be placed from a broker preview of exactly the current fields; (3) one confirmation showing account, symbol, side, quantity and estimated cost; (4) an optional maximum order value. | `app/src/app/trading.rs` module docs [repo] |
| Worker | The UI sends `Request::Trade(TradeRequest::{Preview, Place, Cancel, Portal, Capabilities})` and drains `TradeResponse`, so it never blocks. In the browser the same requests run as tasks on the page's event loop. | `app/src/worker.rs` [repo] |
| Broker crate | `tr-broker`: the SnapTrade client, the `Portfolio`/`Account`/`Position`/`Order` model, `DemoBook`, and `SecretStore` (macOS Keychain service `terminal`, otherwise an owner-only JSON file). `Credentials` has a redacting `Debug` and `mask()`. | `crates/broker/src/*.rs` [repo] |
| Market data | `tr-gateway` (`wss://gateway.rasmic.xyz/ws`) relays Hyperliquid's public websocket. `tr-proto` `MarketInfo { key, venue, kind: Perp \| Spot }`. HIP-3 perps are instruments of their own, labelled `PERP · HL xyz`, never resolved from `AAPL US Equity`. | `crates/proto/src/lib.rs`, `server/src/hl/`, `server.md` §7 [repo] |

The design reuses all of this. Hyperliquid becomes a **third account source** next to Demo and SnapTrade, with its own opt-in, so enabling SnapTrade live orders never enables Hyperliquid orders, or the reverse.

---

## 3. Custody: who holds the key that signs orders

### 3.1 What Hyperliquid requires

- Every order, cancel and modify is an **L1 action** posted to `POST https://api.hyperliquid.xyz/exchange` (testnet: `https://api.hyperliquid-testnet.xyz`) with `action`, `nonce`, `signature`, and optionally `vaultAddress` and `expiresAfter` [doc: EXCH, API]. It can also go over the websocket as a `post` message [doc: WSPOST].
- The signer can be the account's own key **or an API wallet ("agent wallet")** that the account approved. API wallets "are only used to sign"; account data is always queried by the master address [doc: NONCE].
- **Approving an agent is a user-signed action** (`approveAgent`, EIP-712 domain `HyperliquidSignTransaction`), so the main wallet has to sign it once [doc: EXCH, SDK-SIGN]. Approving a builder fee must be signed by the main wallet, "not an agent/API wallet" [doc: BUILDER]. Transfers and withdrawals (`usdSend`, `spotSend`, `withdraw3`) are user-signed actions of the same kind [doc: EXCH]. I infer that an agent key cannot move funds out [inferred]. The one agent-signable transfer, `agentSendAsset`, requires "Destination must match the source address" [doc: EXCH].

So a stolen agent key can trade, and lose money by trading, but per the docs it cannot withdraw. That is the key fact behind the recommendation below.

### 3.2 Options

**Option A: client-side signing with a local API wallet (recommended).**

- Terminal generates a fresh secp256k1 key on the device. Its address is approved as a **named** agent by the user's main wallet, with an expiry. The private key never leaves the device. Every order is signed locally and posted straight to Hyperliquid.
- **Desktop:** the agent key lives in the OS secret store. That is the macOS Keychain, under its own service (`terminal.hyperliquid`, account `testnet:<master address>`). On Linux it is the Secret Service (GNOME Keyring, KWallet) through the `keyring` crate, with **no file fallback**: without a keyring, Hyperliquid trading stays off and Settings says why (Q6). Other platforms have no store, so trading is off there. In this testnet build the desktop approves the agent with a pasted throwaway testnet main key that is wiped after one signature (§4.2); the browser handoff comes before mainnet.
- **Web:** the browser wallet (EIP-1193 `window.ethereum`: MetaMask, Rabby…) signs only the rare user-signed actions: approve agent, approve builder fee, revoke. Orders are signed by the agent key in WASM. The agent key is either **session-only** (memory; lost on reload, re-approve next time) or an **encrypted local key** (wrapped with a non-extractable WebCrypto AES-GCM key and stored in IndexedDB) [inferred design]. Default: session-only, with "Remember on this browser" as an explicit choice.
- Why not have the browser wallet sign each order? L1 actions are signed over an EIP-712 domain with `chainId: 1337` ("phantom agent") [doc: SDK-SIGN]. Wallets generally won't sign typed data for a chain they aren't on, and a popup per order is unusable anyway [inferred]. Hyperliquid's own SDK and docs steer trading processes to API wallets [doc: NONCE].

**Option B: signing on the server.**

- `tr-gateway`, or a new service, holds each user's agent key (in a KMS or HSM) and signs and posts orders for authenticated users.
- For it: one implementation, keys out of browsers, and it is easy to add server-side risk checks.
- Against it:
  1. It reverses a recorded decision. `server.md` §1 says the server "never signs, routes or places orders", partly because of the legal flag [repo].
  2. Goshen Labs would run trading infrastructure that holds keys able to trade customer accounts. That is a much larger security target, and a regulatory question for an Ontario company (§12).
  3. Rate limits are per IP: REST weight is 1200/min, and user-specific websocket subscriptions allow at most 10 unique users per IP [doc: RL]. All users behind one server IP would share them.
  4. It needs sign-in (server.md §8 is still open), per-user secrets at rest, and an audit trail before the first order.
  5. Compromising the server would compromise every user at once.

**Option C (fallback inside A): paste an API wallet key.** For users who already made an API wallet elsewhere (for example on Hyperliquid's own API page), Terminal accepts its private key and stores it like an A key. It has the same trust model, but the key was generated outside Terminal. This is offered as a fallback, not the default.

### 3.3 Recommendation

**Option A.** Every order is signed on the user's own device by a Terminal-generated API wallet that the user's main wallet approves once (named, with an expiry). The key sits in the macOS Keychain on desktop, and in memory or a WebCrypto-encrypted IndexedDB entry on the web. Neither Terminal's server nor Terminal itself ever holds the main key. A leaked agent key can trade but, per the docs, cannot withdraw. Revocation is one action, and `tr-gateway` stays data-only, so the per-IP limits and the server-side custody and legal exposure of Option B never arise.

### 3.4 Signing is small enough to own, and it works in WASM

Hyperliquid warns that hand-rolled signatures fail silently: a wrong signature recovers a different address, with errors like "User or API Wallet … does not exist" [doc: SIGN]. The algorithm is short [doc: SDK-SIGN]:

1. `msgpack(action)`, with **field order exactly as in the SDK** (order matters), then append `nonce` as 8 big-endian bytes, then `0x00` (no vault) or `0x01 ‖ vault address`, and for `expiresAfter` append `0x00 ‖ expiresAfter` as 8 big-endian bytes. `keccak256` of all of it gives `connectionId`.
2. The phantom agent is `{source: "a" (mainnet) | "b" (testnet), connectionId}`.
3. EIP-712 sign `Agent(string source, bytes32 connectionId)` under the domain `{name: "Exchange", version: "1", chainId: 1337, verifyingContract: 0x0}`.
4. User-signed actions (approveAgent, approveBuilderFee) use the domain `HyperliquidSignTransaction`, version 1, with `chainId = signatureChainId` and `hyperliquidChain: "Mainnet" | "Testnet"` in the message. The Python SDK sets `signatureChainId = 0x66eee`, and the docs' examples show `0xa4b1` (Arbitrum) [doc: SDK-SIGN, EXCH].
5. Common pitfalls the docs list: two signing schemes, msgpack field order, trailing zeros in numbers, and upper-case addresses (lowercase everything before signing) [doc: SIGN, TICK].

**Spike [observed, on the box, not in this PR]:** about 40 lines using `alloy-sol-types` (the `sol!` `Agent` struct and `eip712_domain!`), `alloy-signer-local` 1.8 (k256) and `rmp-serde::to_vec_named` reproduce the Python SDK's published test vector `test_l1_action_signing_matches` exactly: r/s/v for both mainnet (v=27) and testnet (v=28). The same crate **builds for `wasm32-unknown-unknown`** with `getrandom` 0.3 `wasm_js` and 0.2 `js` enabled (rustc 1.99.1). I have not yet run it in a browser.

### 3.5 Rust SDK options

| Option | State [observed 2026-11-02] | Fit |
|---|---|---|
| `hyperliquid-dex/hyperliquid-rust-sdk` (official) | 474★, MIT. Last commit 2025-10-21; crates.io `hyperliquid_rust_sdk` 0.6.0 published 2025-05-16. Depends on `tokio` "full" and `tokio-tungstenite` with `native-tls`. Its info client has no `dex` parameter (grep), so no HIP-3 queries. The current docs page links only the Python SDK and the community Rust and TS SDKs [doc: API]. | Not WASM-friendly (deps) [inferred], no HIP-3, and stale. Use as a reference only. |
| `infinitefield/hypersdk` (community; linked from the API docs as the Rust SDK) | 235★, MPL-2.0, pushed 2026-10-26, crates.io 0.3.0 (2026-10-12). alloy 2, rust_decimal, HIP-3 support, multisig. | **Does not build for wasm32** [observed]: even with `default-features = false`, 0.3.0 failed with 5 errors (yawc `TcpWebSocket`, non-`Send` futures, `reqwest::ClientBuilder::timeout`). A good reference and a source of test vectors; possible on desktop only. MPL-2.0 is file-level copyleft (fine to depend on; copying files in carries the licence). |
| `alethea` | The only crate by that name on crates.io is a templating library (0.1.0), unrelated to Hyperliquid [observed]. If Micky meant another crate, it needs a pointer (Q9). | Not applicable. |
| Own minimal module on `alloy-sol-types` + `alloy-signer-local` + `rmp-serde` | Proven by the spike above. | **Recommended.** One code path for desktop and WASM, our own action structs with explicit field order, and CI pinned to Python SDK test vectors. |

---

## 4. User flow

### 4.1 Turning it on

1. Settings › Trading › **Hyperliquid** (new section; off by default). This build has **testnet only**: no network selector, no mainnet URL, no switch. Mainnet comes in phase 4, after §12, in a build made for it (§8).
2. A first-run acknowledgement like the SnapTrade `enable_dialog` [repo], with Hyperliquid-specific lines: orders are irreversible on-chain transactions; perps are leveraged and can be liquidated; HIP-3 stock markets are perpetual futures, not shares; Hyperliquid's Terms restrict some regions, including Ontario (§12), and you are responsible for whether you may use it.
3. **Connect wallet**: the master address, used for reads and display.
   - Web: EIP-1193 `eth_requestAccounts`.
   - Desktop: paste a read-only address (v1), or approve with a pasted testnet throwaway key (§4.2). A read-only address alone already lights up `PORT`/`POS`/`ORD` with live data and no trading.

### 4.2 Approving the agent (one main-wallet signature)

1. Terminal generates the agent key locally and stores it (Keychain or browser).
2. It builds `approveAgent { hyperliquidChain, signatureChainId, agentAddress, agentName: "terminal-<device> valid_until <ms>", nonce }`.
   - **Named, never unnamed.** "An account can have 1 unnamed approved wallet and up to 3 named ones", plus 2 more named per sub-account. Approving a new unnamed agent deregisters the existing unnamed one [doc: EXCH, NONCE], which may be the user's Hyperliquid web session or another tool.
   - **Expiry:** `valid_until`, at most 180 days ahead [doc: EXCH]. Default: 30 days (Q5).
3. Signing:
   - Web: `eth_signTypedData_v4` with the connected wallet. `signatureChainId` = the wallet's current chain id, because wallets reject typed data for another chain [inferred; verify in phase 1].
   - Desktop, **v1 (testnet only):** the user pastes a throwaway testnet main key into a masked field. Terminal signs `approveAgent` with it once, wipes the buffer and never stores it. The Settings copy says, in amber, never to paste a key that holds real funds.
   - Desktop, later (before mainnet): Terminal opens `https://terminal.rasmic.xyz/app/#hl-approve?agent=0x…&name=…&net=testnet`. Only the **public** agent address is in the URL fragment, which never reaches a server. The web page has the main wallet sign `approveAgent` and posts it to `/exchange`. Desktop then polls `{"type": "extraAgents", "user": master}` [doc: SDK-INFO] and `userRole` [doc: INFO] until the agent appears.
4. Terminal shows: agent address (masked), name, network, expiry, and "Revoke".

### 4.3 Revoking and rotating

- The docs list no explicit "revoke agent" action. An agent is deregistered when a new `approveAgent` reuses its **name** (or a new unnamed one is approved), when it expires, or when the account runs out of funds [doc: NONCE].
- **Revoke** in Terminal: delete the local key first (Keychain or IndexedDB), then have the main wallet sign `approveAgent` with the **same name** and a freshly generated throwaway address whose key is dropped immediately. Whether Hyperliquid accepts the zero address for this is [unverified]; test it in phase 1.
- **Never reuse an agent address.** The docs "strongly" advise it, because once pruned, the nonce set can let "previously signed actions … be replayed" [doc: NONCE]. Rotation always generates a new key.
- Terminal warns 7 days before expiry and offers one-click renew (new key, same name).

### 4.4 Funding

Out of scope for Terminal v1. The user deposits on Hyperliquid themselves. Account abstraction matters for HIP-3:
- In **unified account** mode, "USDC balance is the single source for validator-operated perps, XYZ perps, and spot".
- In **standard** mode, each dex has a separate balance [doc: AAM].

Terminal reads the mode and shows per-dex balances in standard mode. If the HIP-3 dex balance is zero, the ticket explains it instead of failing at submit.

---

## 5. Order routing: perps, spot and HIP-3 dexes

### 5.1 Asset ids [doc: ASSET, EXCH]

| Market | Asset id | Example |
|---|---|---|
| Core perp | index in `meta.universe` | `BTC` = 0 on mainnet |
| Spot | `10000 + spotMeta.universe[i].index` | `PURR/USDC` = 10000. HYPE spot is 107 on mainnet and 1035 on testnet: **ids differ by network** |
| HIP-3 perp | `100000 + perp_dex_index * 10000 + index_in_meta`, names always `{dex}:{coin}` | see below |

`perp_dex_index` is the position in the `perpDexs` info response (index 0 is the core dex) [doc: ASSET, PERP]. Observed on 2026-11-02:

| Network | dex (index) | `xyz:AAPL` | Others |
|---|---|---|---|
| Mainnet | `xyz` (1), `para` (8), `mkts` (9), `io` (10) | index 9 in `meta{dex:"xyz"}` → **110009** | `para:TOTAL2` → 180000, `mkts:US500` → 190000, `io:OAI` → 200000 |
| Testnet | `xyz` (**65**); `para`, `io`, `mkts` absent | index 9 → **750009** | — |

**Rule: never hard-code asset ids.** `tr-hl` builds an `AssetRegistry` per network from `perpDexs` + `meta{dex}` per dex + `spotMeta`. It caches with a TTL and refreshes on an unknown coin or a reject. It is keyed by the venue-native names `tr-proto` already uses (`BTC`, `xyz:AAPL`, `@107`, `PURR/USDC`) [repo]. `szDecimals`, `maxLeverage`, `onlyIsolated` and `marginMode` come from the same `meta` (for `xyz:AAPL`: `szDecimals` 3, `maxLeverage` 20 [observed]).

### 5.2 Price and size rules [doc: TICK]

- Prices: at most 5 significant figures, and at most `6 − szDecimals` decimals for perps or `8 − szDecimals` for spot. Integer prices are always valid.
- Sizes are rounded to `szDecimals`. Trailing zeros are stripped before signing.
- Minimum order value: $10 [doc: EXCH, error example].

The ticket rounds as you type and shows the value that will be sent.

### 5.3 Margin and leverage

`updateLeverage {asset, isCross, leverage}` [doc: EXCH]. The ticket shows the current leverage and margin mode from `activeAssetData` [doc: WSSUB]. Changing it is its own confirmed action, never a side effect of placing an order. HIP-3 assets may be isolated-only (`onlyIsolated`) [doc: PERP, HIP3]. Terminal respects the meta flag and caps leverage at `min(meta.maxLeverage, user cap)` (§8).

---

## 6. Order types and the ticket

### 6.1 Mapping

| Ticket | Hyperliquid action | Notes |
|---|---|---|
| Limit + GTC / IOC / Post-only | `order` with `t: {limit: {tif: "Gtc" \| "Ioc" \| "Alo"}}` | ALO "will be canceled instead of immediately matching" [doc: EXCH] |
| Market | `order` with an IOC limit at `mid × (1 ± slippage)` | There is no native market order. The Python SDK's `market_open` uses an IOC limit and a default 5% slippage [doc: SDK-EXCH]. Terminal default: 1%, editable, shown in the confirm. |
| Stop / Stop-limit, Take-profit | `order` with `t: {trigger: {isMarket, triggerPx, tpsl: "sl" \| "tp"}}` | |
| Bracket (entry + TP + SL) | `order` batch with `grouping: "normalTpsl"`; position TP/SL with `"positionTpsl"` | phase 3 |
| Reduce-only | `r: true` | Default on for close and `SELL ALL` from a position row |
| Client order id | `c`: 128-bit hex `cloid` | Always set, so an uncertain placement can be reconciled (like `PlaceError::uncertain` today [repo]) |
| Modify | `modify` / `batchModify` | Only from `ORD` |
| Cancel | `cancel` (by `oid`) or `cancelByCloid` | `f: true` (fast) is rejected for trigger orders [doc: EXCH]. Terminal leaves `f` out and pins the exact bytes with SDK test vectors, since field presence changes the hash [inferred] |
| Expiry | `expiresAfter = now + 30 s` on every order | A stale signed order can't land late. Expired actions "consume 5x the usual address-based rate limit" [doc: EXCH], so the window is short but not tiny. |
| Not in v1 | `twapOrder`, `trailingStop`, outcomes, vault/sub-account `vaultAddress` | |

`TimeInForce::Day` and `Fok` have no Hyperliquid equivalent. The ticket hides them for HL accounts.

### 6.2 The ticket in the Bloomberg-style panes

The existing `Ticket` window gains an HL variant, chosen by the account, not a second ticket:

```
┌ BUY  xyz:AAPL ─ Apple Inc perp ─ PERP · HL xyz ─────────── TESTNET ┐
│ Account   HL 0x12…ab (unified)         Equity $1,024.10  Avail $812 │
│ Side      [BUY] SELL       Type  Market  Limit  Stop  TP           │
│ Size      0.500 AAPL   ≈ $113.20      [units | $]                  │
│ Limit     226.40        TIF  GTC  IOC  Post-only                   │
│ Leverage  5× cross (max 20×)   Reduce-only [ ]   Slippage 1.0%     │
│ Est. fee  $0.05   Liq. px ≈ 184.9   Margin $22.64                  │
│ ⚠ Perpetual future on Hyperliquid (xyz). Not a share of AAPL.      │
│                                    [ Review order  ⏎ ]             │
└────────────────────────────────────────────────────────────────────┘
```

- **Network badge** in the title: `TESTNET` in amber, `MAINNET` in the live red that `LIVE` uses today [repo].
- The market line reuses the label code that already renders `PERP · HL xyz` [repo]. HIP-3 rows add a one-line perp warning that cannot be dismissed (§7).
- Reference price, fee and liquidation estimates come from the same network the order goes to. On testnet that is testnet `allMids`/`l2Book`, **not** the gateway's mainnet quotes [inferred: testnet prices differ from mainnet].
- Commands: `BUY xyz:AAPL 0.5 @226.4`, `SELL BTC 0.01 MKT`, `TRADE HYPE/USDC`, with the same parser as today. A new `KILL` command and the Kill button are covered in §8.

### 6.3 Confirmation

Every HL order and cancel-all goes through **one confirmation modal**. There is no one-click (that stays demo-only). It shows:

- network;
- master address (masked) and agent name;
- market with its kind (perp/spot/HIP-3 dex);
- side, size, price or slippage cap;
- notional;
- leverage and margin mode;
- estimated liquidation price;
- fee, including any builder fee;
- reduce-only;
- `expiresAfter`.

Editing any field invalidates the confirmation, mirroring today's "preview of exactly the ticket's current fields" rail [repo]. Hyperliquid has no preview endpoint, so Terminal builds the preview locally from `activeAssetData` and `clearinghouseState`. It is labelled as an estimate.

---

## 7. HIP-3 stock perps: labelled as perps, not shares

The rules from `server.md` §7 carry over to trading [repo]:

1. A HIP-3 market is its own instrument (`xyz:AAPL`). `BUY AAPL` on an HL account **does not** route to `xyz:AAPL`. It errors with a suggestion: "AAPL is a stock. Hyperliquid has the perp xyz:AAPL (PERP · HL xyz). Trade that instead?"
2. Everywhere the market appears (ticket, confirm, `POS`, `ORD`, fills, toasts), the `PERP · HL <dex>` label is shown. Positions use `InstrumentKind::Future` (no new variant needed) and show "perp" in the description, never "shares".
3. The confirm repeats that it is a leveraged perpetual future with funding, priced by the deployer's oracle, which can diverge from the listed stock (off-hours, opens, earnings, corporate actions). The deployer can halt and settle the market at mark [doc: HIP3]. Cross margin across dexes carries extra risk [doc: HIP3].
4. `PORT` totals keep HL perps apart from brokerage share holdings. There is no "AAPL exposure" netting across a share and a perp in v1.

---

## 8. Safety

| Rail | Design |
|---|---|
| Off by default | `prefs.trading.hl.enabled = false`, separate from SnapTrade's `enabled`. The Hyperliquid account is read-only until enabled with its acknowledgement (§4.1). |
| Testnet only (v1) | `Network` has one variant, `Testnet`. The endpoints are the testnet hosts, the phantom agent's `source` is always `"b"` and `hyperliquidChain` always `"Testnet"`; a stored `"network": "mainnet"` reads as testnet. Tests grep `tr-hl` for mainnet hosts. Mainnet needs a later build after §12, with its own acknowledgement. The amber TESTNET badge is on the toolbar, the account, the ticket and every confirmation. |
| Confirmation | One per order, cancel-all and leverage change (§6.3). Enter confirms; Escape or a click outside cancels, as today [repo]. |
| Hard caps | Constants in `tr-hl`, not settings: **$100** per order (size × the higher of the order price and the mid), **3×** leverage, **1%** market slippage. `tr-hl` checks them when the confirm opens, and the worker reads the testnet mid and leverage again and re-checks right before signing. An order is refused while the account's leverage on that market is over 3× (Hyperliquid's default is 20×), so a confirmed "Set 3×" comes first; reduce-only orders are not blocked by leverage. A breach refuses before anything is signed. |
| Kill switch | `KILL` or the red KILL button in `ORD` (and in Settings): one confirmation, then Hyperliquid orders turn off in prefs and every open order is cancelled, one `cancel` action per dex (cancels have a higher rate-limit allowance [doc: RL]). "And flatten" is not in v1. |
| Dead man's switch | **Opt-in, off by default** (Q7). While on and trading is enabled, Terminal keeps a `scheduleCancel` about 60 s ahead and refreshes it every 30 s [doc: EXCH]; turning it off clears the schedule. Constraints: at least 5 s ahead, and at most 10 triggers per day, reset 00:00 UTC [doc: EXCH]. |
| Nonces | One agent per device and network (nonces are tracked per signer; "use a separate private key" per session [doc: NONCE]). The nonce is an atomic `max(last + 1, now_ms)`, within (T − 2 days, T + 1 day) [doc: NONCE]. A noop (`{"type": "noop"}`) can invalidate an in-flight nonce [doc: EXCH]. |
| Uncertain results | A network error after send means "uncertain", never "failed". Reconcile by `cloid` via `orderStatus` and `orderUpdates` before allowing a resend (same idea as `PlaceError::uncertain` [repo]). |
| Never log keys | `AgentKey` is a newtype with redacted `Debug`/`Display` (like `Credentials` and `mask()` [repo]) and is zeroized on drop. It is never serialized into prefs, layouts or the demo JSON. Signed request bodies are not logged; logs carry `cloid`, `oid`, asset and status only. Unit test: a `tracing` capture of a full place/cancel cycle contains no 64-hex-digit secrets. |
| Rate limits | Address-based limit: 1 request per 1 USDC traded since inception, plus an initial buffer of 10,000; when limited, 1 request every 10 s [doc: RL]. A human-driven terminal stays far below this. Terminal reads `userRateLimit` [doc: INFO] and surfaces it in Settings, and backs off quietly on 429s, as the SnapTrade capabilities code does [repo]. |

---

## 9. Positions, orders, fills and PnL from the user streams

The client connects **directly** to `wss://api.hyperliquid.xyz/ws` (testnet: `wss://api.hyperliquid-testnet.xyz/ws`) [doc: WS] for the user's own data, not through `tr-gateway`. That keeps user addresses off our server and fits the 10-users-per-IP limit [doc: RL]. The REST endpoints answer CORS preflight with `access-control-allow-origin: *` on mainnet and testnet [observed], so the web build can post `/exchange` and `/info` directly.

| Subscription [doc: WSSUB] | Feeds |
|---|---|
| `clearinghouseState {user, dex}` per used dex, or `allDexsClearinghouseState {user}` | `POS` rows (szi, entryPx, positionValue, unrealizedPnl, returnOnEquity, liquidationPx, marginUsed, cumFunding, leverage), `ACCT` (marginSummary, crossMarginSummary, withdrawable) [doc: PERP] |
| `spotState {user}` | spot balances (in unified/portfolio-margin modes, "all balances and holds" live in the spot clearinghouse state [doc: AAM]) |
| `openOrders {user, dex}` and `orderUpdates {user}` | `ORD` (status, limitPx, sz, origSz, oid, cloid) |
| `userFills {user}` | `ACTV` fills, realized PnL (`closedPnl`), fees (`fee`, which includes `builderFee`) [doc: INFO] |
| `userEvents {user}` (channel `user`) | fills, funding, liquidations, `nonUserCancel`: toasts |
| `userFundings`, `userNonFundingLedgerUpdates` | funding and deposit/transfer history in `ACTV` |
| `activeAssetData {user, coin}` | the ticket: leverage, max trade sizes, available to trade (perps only) |
| `webData3 {user}` | optional aggregate. The current docs list `webData3`, not `webData2`, and warn that undocumented fields "will be removed" [doc: WSSUB]. Terminal uses the typed streams above. |
| `notification {user}` | toasts |

The streaming endpoints send a snapshot first with `isSnapshot: true` [doc: WSSUB]. After a reconnect, Terminal replaces state from the snapshot and dedupes fills by `(time, coin, tid)` (the docs give that as the globally unique trade id [doc: WSSUB]).

**Model mapping.** A new `tr-hl` adapter produces the existing `tr_broker::Portfolio`:
- `Account { institution: "Hyperliquid", kind: "Perps" | "Spot", is_paper: network == Testnet, number: masked address }`.
- `Position { kind: Future | Crypto, units: szi (signed), cost_basis: entryPx, price: mark }`.

So `PORT`/`POS`/`ORD`/`ACTV` render HL with modest UI change, **but the market link needs real work**:

- Today `tr_broker::market_key()` returns `None` for `InstrumentKind::Future` [repo: `crates/broker/src/model.rs`].
- For `Crypto` it builds a spot-style pair (`BTC` → `BTCUSD`), not a Hyperliquid perp key [repo].
- `PortfolioPanes::live_price` looks quotes up only by that key [repo: `app/src/app/broker.rs`].
- The position-row Buy/Sell/Close controls are limited to `Stock | Etf` [repo: `app/src/app/portfolio_view.rs`].

As modelled above, an HL perp row would have no live repricing, no chart link and no trade buttons. The integration therefore adds:

1. **A venue-native market key on `Position`.** A new optional, serde-defaulted field (`market: Option<String>`) that the HL adapter fills with the exact key the gateway publishes in `MarketInfo.key` (`xyz:AAPL` for HIP-3, the core-perp and spot keys as `tr-proto` names them) [repo]. `Position::market_key()` returns it before falling back to today's kind-based mapping, so SnapTrade and demo rows are unchanged. HIP-3 keys resolve through `Security::perp` (`AssetClass::Perp`) [repo: `crates/core/src/security.rs`], never through the stock path.
2. **Repricing from that key, kept separate per network.** Mainnet HL rows reprice through today's path: `live_price` reads the shared quote map, which `broker_periodic` fills from the gateway [repo]. Testnet rows **must not** use that map. Testnet and mainnet use the same market keys (`xyz:AAPL`), so either network's price could overwrite the other's. The plan:
   - a separate `testnet_quotes` map owned by the HL adapter, filled only from testnet `allMids` (per dex);
   - `live_price` takes the account's network (from `Account.is_paper` / the HL account's network) and reads only the matching map;
   - `broker_periodic` requests gateway quotes only for keys from mainnet accounts (and SnapTrade/demo), never for testnet rows;
   - the ticket's reference price, fee and liquidation estimates use the same network-selected source (§6.2);
   - tests feed interleaved mainnet and testnet updates for the same key and assert each row and ticket reads only its own network.
3. **Row actions.** The `tradable` check in `portfolio_view.rs` also allows rows from an enabled HL account that have a `market`. Close sends a reduce-only order (§6.1), through the same confirmation (§6.3). Chart and key menus use `market`.
4. **Tests.** Unit tests for `market_key()` precedence (an HL row with `market` set, a SnapTrade `Future` row still `None`), and `live_price` tests with perp quotes on both networks.

Unrealized PnL is then repriced from the gateway's live marks on mainnet, or testnet mids on testnet, as the panes already do for SnapTrade rows that have a key [repo]. Realized PnL = Σ `closedPnl` − Σ `fee` over the selected range, with funding shown separately.

---

## 10. Code layout (as built in v1; see §16)

- `crates/hl` (`tr-hl`): `signing` (§3.4), `actions` (order, cancel, cancelByCloid, modify, batchModify, updateLeverage, scheduleCancel, noop, approveAgent, approveBuilderFee, structs with explicit field order), `assets` (the registry, §5.1), `rounding` (§5.2), `client` (`/info` and `/exchange` over `reqwest`, which already compiles for wasm in this workspace [repo]), `stream` (user websocket), `nonce`. No `tokio` "full"; it follows `tr-broker`'s wasm split [repo].
- `tr-broker`: `secrets` gains a second store for agent keys (Keychain service `terminal.hyperliquid`). The existing SnapTrade item is untouched.
- `tr-broker` `model`: `Position.market: Option<String>` (serde default) and the `market_key()` precedence change from §9. `app/src/app/portfolio_view.rs`: the HL row actions. `app/src/app/broker.rs`: a network-aware `live_price` with a separate testnet quote map, and `broker_periodic` requesting gateway quotes only for mainnet keys (§9).
- `app`: `TradingPrefs.hl: HlPrefs { enabled, network, max_order_value, max_leverage, daily_notional_cap, slippage_bps, dead_mans_switch, agent: Option<AgentMeta> }`. `AgentMeta` holds the public address, name, expiry and network, never the key. New `TradeRequest::Hl(HlRequest)` and `TradeResponse::Hl(..)` variants in the worker. The ticket gets an HL variant, and Settings › Trading gets a Hyperliquid section.
- `tr-proto` / `tr-gateway`: **no change.** Market data keeps flowing as today. User data and orders never touch the server.

---

## 11. Testing plan (testnet only, evidence-driven)

**No orders are placed in this design step**, on mainnet or testnet. From phase 1 on, every trading test runs on **testnet only**, until Micky signs off on §12 and phase 4.

1. **Unit (CI, offline):**
   - Signing matches the Python SDK vectors (`test_phantom_agent_creation_matches_production`, `test_l1_action_signing_matches`, `…_order_matches`, `…_order_with_cloid_matches`, `…_matches_with_vault`, `…_tpsl_order_matches`, `test_schedule_cancel_action`, and the user-signed `test_sign_usd_transfer_action`; the SDK has no approve-agent vector, so we generate one with the Python SDK and pin it) [doc: SDK-TEST], natively and under `wasm-bindgen-test`.
   - msgpack field order, rounding (§5.2) and asset ids from recorded `perpDexs`/`meta` fixtures.
   - The no-secrets-in-logs test.
2. **Recorded fixtures:** JSON from testnet `/info` and websocket snapshots, so parser tests don't need the network. This follows server.md's fixture approach [repo].
3. **Testnet integration (manual and scripted, gated by an env flag, never in CI):**
   - approve agent → place a $10–20 GTC limit far from the market on `BTC` and on testnet `xyz:AAPL` (750009) → confirm it shows in `ORD` → modify → cancel → kill switch with two resting orders → revoke agent and prove the old key is rejected;
   - then one small IOC fill to see `userFills`, `POS` and PnL.
   - Testnet lacks `para`, `io` and `mkts` [observed]; those dexes are covered by asset-id fixture tests only, until mainnet.
4. **Evidence for each PR:**
   - **before-and-after screenshots** of the ticket, confirm, `POS`, `ORD`, `ACTV` and Settings › Trading, on desktop and web;
   - the testnet `oid`s and the explorer links for each action;
   - the list of commands run. No keys, addresses in full, IPs or `.env` contents appear in PRs or logs.
5. **Browser:** the web build signs on testnet from MetaMask and from Rabby (approve agent), and from the WASM agent key (orders).
6. **Failure drills:** kill the network mid-order (an uncertain result reconciled by `cloid`), let `expiresAfter` lapse, hit the max-notional guard, use an expired agent, and run the dead man's switch firing.

**Testnet funding caveat:** the faucet gives 1,000 mock USDC only to addresses that "have deposited on mainnet with the same address" [doc: FAUCET]. For an Ontario resident, that mainnet deposit is itself a ToS question (§12, Q2).

---

## 12. Legal and Terms of Use (needs Micky)

- **Terms of Use, last updated June 15, 2026, §1.6:** "Restricted Persons include: (a) persons or entities who reside in, are located in, are incorporated in, or have a registered office in the United States of America or **Ontario, Canada**; (b) … jurisdictions subject to applicable economic and trade sanctions …; and (c) citizens of Restricted Territories" [doc: TOU].
- **§3.1.5** prohibits circumvention: "using technologies such as VPNs, proxies, or other methods to conceal your location" or "making false statements … about your residency" [doc: TOU].
- **§1.9:** the user is responsible for compliance, "including … laws governing leveraged or derivative trading" [doc: TOU].
- **Scope:** the Terms govern Hyperliquid Corp.'s website Interface at `app.hyperliquid.xyz`. They say the Interface "is not the exclusive means of accessing Hyperliquid" (§1.1) [doc: TOU]. They don't directly address third-party clients or the API. **That is not a green light.** Building a client that lets Ontario or US residents trade where the official interface may not is exactly what needs legal review. The design must not geo-dodge, hide location or tell users how to get around restrictions.
- **Micky and Goshen Labs are in Toronto.** Micky personally trading on mainnet, and Goshen shipping order entry to Ontario or US users, are separate questions, and both need advice. Collecting builder fees (§13) could make Terminal look like it is in the business of trading, which matters under Ontario securities law [inferred; not legal advice].
- **Proposed product guardrails, whatever the outcome:**
  - Hyperliquid trading off by default (§8);
  - a jurisdiction attestation in the enable dialog ("I am not a Restricted Person under Hyperliquid's Terms"), with a link to the Terms;
  - no circumvention guidance;
  - an option to compile HL trading out of a build entirely (`hl-trading` cargo feature) if counsel says so.

---

## 13. Builder codes (monetisation option) [doc: BUILDER, EXCH]

- An order may carry `builder: {b: address, f: tenths of a bp}`. For example, `f: 10` = 1 bp. Up to 5 sub-builders can share the fee.
- Caps: 0.1% on perps and 1% on spot. Builder fees only apply to fees collected in the quote or collateral asset, so not to the buying side of spot.
- The user must first sign `approveBuilderFee {maxFeeRate, builder}` with the **main wallet**, not an agent, and can revoke. A user can have at most 10 active approvals.
- The builder address needs at least 100 USDC in perps account value and must be in standard abstraction mode to accrue fees [doc: AAM].
- Fees are claimed through referral rewards. Per-day builder fills are published at `stats-data.hyperliquid.xyz/Mainnet/builder_fills/{address}/{YYYYMMDD}.csv.lz4`, with the address lowercase.
- **Design:** the builder code is off in v1 (`f` absent). If Micky wants it, it is a separate opt-in screen showing the rate (e.g. 1–5 bp) before the main wallet signs, and the confirm always shows the builder fee. It is subject to §12 (Q1).

---

## 14. Phased milestones (small PRs, each proving one thing)

| Phase | Scope | Exit evidence |
|---|---|---|
| 0 | This doc, reviewed. | Micky's answers to §15 |
| 1 | `tr-hl` signing, actions, assets and rounding, plus SDK vector tests (native + wasm). Read-only: connect an address, then `POS`/`ORD`/`ACTV`/`ACCT` from the user streams (testnet and mainnet reads), including the `Position.market` key so perp rows reprice live and link to charts (§9). | CI green, screenshots of the read-only panes |
| 2 | Agent lifecycle on **testnet**: generate, Keychain/IndexedDB storage, approve (web wallet + desktop handoff), expiry warning, revoke. | testnet `extraAgents` before and after; a revoked key is rejected |
| 3 | Testnet order entry: limit/market(IOC)/stop/TP, reduce-only, modify, cancel, kill switch, dead man's switch, max-notional and leverage guards, confirm modal, HIP-3 labelling. | §11 drills, before-and-after screenshots, testnet oids |
| 4 | Mainnet behind `hl-mainnet` + acknowledgement, **only after §12 is resolved**. Tiny-size canary trades by Micky personally. | Legal sign-off recorded in this doc |
| 5 | Optional: builder code, brackets (`normalTpsl`), spot↔perp `usdClassTransfer`, Linux Secret Service. | per feature |

---

## 15. Open questions for Micky

**Decided (2026-11-02).** Micky: "Build it on testnet now, we'll sort the legal review before mainnet." Defaults for v1:

1. Signing on the device only (Option A). `tr-gateway` stays data-only; no server change.
2. A named API wallet, `terminal-<device> valid_until <ms>`, valid 30 days by default (180 at most). Never the unnamed slot, never a reused agent address.
3. Desktop keys in the macOS Keychain; on Linux the Secret Service with no file fallback (no keyring means no trading, and Settings says why). Windows: no store in this build, so trading stays off there.
4. Web keys for the session only (memory), with an opt-in "remember on this browser" that encrypts with a non-extractable WebCrypto AES-GCM key kept in IndexedDB.
5. Perps only: the core dex and the HIP-3 `xyz` dex. No spot; `para`, `io` and `mkts` are refused at the ticket with a reason.
6. Hard caps enforced in `tr-hl` before signing, not raisable in this build: $100 per order, 3× leverage, 1% market slippage.
7. The dead man's switch is opt-in, off by default.
8. One confirmation for every order, modify, cancel-all and leverage change, plus the kill switch (`KILL` and a button in `ORD`).
9. No builder fee: the `builder` field is never sent.
10. Hyperliquid sits inside LIVE next to the SnapTrade accounts, with an amber TESTNET badge on the account, the ticket, the confirmation and the toolbar.
11. Asset ids come from the API per network (`perpDexs` and `meta` per dex), never hard-coded.
12. A separate testnet quote map: testnet rows and tickets never read the gateway's mainnet quotes, and the reverse.

The questions as first asked:

1. **Legal / ToS (blocking for mainnet):** Hyperliquid's Terms restrict persons in Ontario (§12). Who reviews (a) you personally trading, and (b) Goshen Labs shipping HL order entry to users in Ontario and the US? Until then, is a testnet-only build OK to develop and demo?
2. **Testnet funds:** the faucet requires a prior mainnet deposit from the same address. Do you already have a mainnet-funded address you're comfortable using for testnet, or should we find another source of testnet USDC (e.g. a teammate outside restricted regions sending testnet USDC) [unverified that this works]?
3. **Custody:** OK with Option A (local API wallet, main wallet signs once, server never signs)? Or do you want Option B for a future hosted or mobile product?
4. **Web key storage:** default to session-only agent keys on the web (re-approve each visit), with an opt-in encrypted IndexedDB key? Or remember by default?
5. **Agent expiry:** 30 days by default (max 180)?
6. **Linux:** require Secret Service (libsecret) for the agent key, or allow today's owner-only file fallback with a warning?
7. **Dead man's switch:** on by default while trading is enabled (cancels *all* the account's open orders if Terminal disappears, including ones placed elsewhere), or opt-in?
8. **Limits:** defaults of $100 max order notional on testnet, 3× max leverage, 1% market slippage. What should the mainnet defaults be, and should the cap be hard (not editable above a ceiling)?
9. **"alethea":** the only `alethea` crate on crates.io is a templating library. Did you mean a different signing crate? (The recommendation is our own small module on alloy, §3.5.)
10. **Builder fees:** monetise with a builder code (e.g. 1–5 bp on perps, opt-in, always shown in the confirm), or keep Terminal fee-free? This depends on Q1, since fees may change the regulatory picture.
11. **Scope of "stocks":** HIP-3 perps on `xyz` only for v1, or also `para`/`io`/`mkts` (which testnet doesn't have, so they'd launch untested end to end)?
12. **Spot:** include HL spot (e.g. `HYPE/USDC`) in v1, or perps only?
13. **One account mode or two?** Should the toolbar badge become `DEMO` / `LIVE` / `HL` (three modes), or should HL accounts sit inside `LIVE` next to SnapTrade accounts, with the network badge on each?

---

## References

- **API**: API overview (SDK links, testnet URL). https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api
- **EXCH**: Exchange endpoint (order, cancel, cancelByCloid, scheduleCancel, modify, batchModify, updateLeverage, approveAgent, approveBuilderFee, noop, expiresAfter). https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint
- **SIGN**: Signing. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/signing
- **NONCE**: Nonces and API wallets. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets
- **ASSET**: Asset IDs. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids
- **TICK**: Tick and lot size. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size
- **RL**: Rate limits and user limits. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits
- **INFO**: Info endpoint (userRole, userRateLimit, maxBuilderFee, fills). https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint
- **PERP**: Info endpoint, perpetuals (perpDexs, meta, clearinghouseState). https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals
- **WS**: Websocket. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket
- **WSSUB**: Websocket subscriptions. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions
- **WSPOST**: Websocket post requests. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/post-requests
- **BUILDER**: Builder codes. https://hyperliquid.gitbook.io/hyperliquid-docs/trading/builder-codes
- **AAM**: Account abstraction modes. https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes
- **HIP3**: HIP-3 builder-deployed perpetuals. https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals
- **FAUCET**: Testnet faucet. https://hyperliquid.gitbook.io/hyperliquid-docs/onboarding/testnet-faucet
- **TOU**: Hyperliquid Terms of Use (last updated June 15, 2026). https://app.hyperliquid.xyz/terms
- **SDK-SIGN**: Python SDK `hyperliquid/utils/signing.py`. https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/utils/signing.py
- **SDK-EXCH**: Python SDK `hyperliquid/exchange.py` (`market_open`, `DEFAULT_SLIPPAGE`, `approve_agent`). https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/exchange.py
- **SDK-INFO**: Python SDK `hyperliquid/info.py` (`extra_agents`). https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/info.py
- **SDK-TEST**: Python SDK `tests/signing_test.py`. https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py
- Rust SDKs: https://github.com/hyperliquid-dex/hyperliquid-rust-sdk, https://github.com/infinitefield/hypersdk

### Observations log (read-only, 2026-11-02 ~23:30 UTC)

- `POST /info {"type":"perpDexs"}` mainnet: index 1 `xyz`, 8 `para`, 9 `mkts`, 10 `io` (11 entries). Testnet: 277 entries, `xyz` at 65, no `para`/`io`/`mkts`.
- `meta {dex:"xyz"}`: mainnet 142 assets, `xyz:AAPL` index 9 (`szDecimals` 3, `maxLeverage` 20), collateral token 0. Testnet 70 assets, `xyz:AAPL` index 9.
- `OPTIONS /exchange` with `Origin: https://terminal.rasmic.xyz`: mainnet and testnet both return `access-control-allow-origin: *`.
- Signing spike: alloy-signer-local 1.8.3 + alloy-sol-types + rmp-serde. Matches SDK vector r/s/v on mainnet and testnet; `cargo build --target wasm32-unknown-unknown` OK.
- `hypersdk` 0.3.0 with `default-features = false` on wasm32: 5 compile errors in `hypersdk` itself.

---

## 16. v1 as built (testnet)

- **`crates/hl` (`tr-hl`):** `network` (testnet only), `key` (`AgentKey`, `MainKey`: redacted, zeroized on drop), `signing` (k256 directly, with `alloy-primitives` and `alloy-sol-types`; no alloy signer crates, which pull in the RPC stack), `actions`, `assets`, `rounding`, `caps`, `nonce`, `trader` (the only way to an L1 signature), `client` (reqwest, native and wasm), `model`, `stream` (subscriptions and a reducer; the socket is the app's). It builds for wasm32.
- **`tr-broker`:** `AgentKeyStore` (Keychain, Secret Service or memory; never a file), and `market: Option<String>` on `Position`, `Order` and `Activity`, which `market_key()` prefers.
- **App:** `app/src/hl_worker.rs` (the user stream, reads, signing after the re-check, the key store off the UI thread), `app/src/app/hl.rs` (state, flows, rows, testnet quotes), `hl_view.rs` (ticket body, confirmations, Settings), `hl_web.rs` (EIP-1193 wallet, IndexedDB key). The ticket's Hyperliquid variant covers market (IOC, 1% cap), limit (GTC, IOC, post-only), stop loss and take profit (reduce-only market triggers), reduce-only and "Set 3×".
- **Checked on testnet** with throwaway keys and no funds: every signature recovers its signer, and a corrupted one recovers someone else. `approveAgent` needs a deposit first ("Must deposit before performing actions"). The PR that added this build lists the responses.
- **Not done in v1:** modify is in `tr-hl` and the worker but has no `ORD` control yet; brackets (`normalTpsl`), "and flatten" on KILL, the desktop browser handoff, spot, other HIP-3 dexes, the daily notional cap, and the expiry warning's one-click renew. Resting orders, fills, positions and P&L can only be checked once the testnet account has test USDC.
