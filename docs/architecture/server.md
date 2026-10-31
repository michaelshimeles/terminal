# Terminal data server: design

Status: **draft for review**. The first server code runs on an interim public feed (see [Interim: public feed](#interim-public-feed)).
Author: Grok Bot, for Micky. Research date: 2026-10-29. Times are UTC unless marked.

Terminal today fetches all of its own data: Yahoo, Binance and Coinbase websockets, Treasury.gov, Senate eFD and the House Clerk, and SnapTrade. Each desktop process opens its own connections. The browser build serves sample data wherever a source blocks cross-origin requests (`TODO(web proxy)` in `crates/data/src/web.rs` and `crates/congress/src/db_web.rs`). This doc proposes the replacement Micky asked for: **one Rust server that every client (desktop, web, later mobile) connects to, so market data and user state sync in real time**. Hyperliquid is the first market-data source, and the design leaves a clean provider interface for licensed equities and options later.

**Decision (Micky, 2026-10-29): Terminal runs on our own Hyperliquid non-validator node(s) and our own systems from day one.** Live data comes only from the node's output files and its local info server: books, trades, candles, mids, marks, oracle prices and funding. The public Hyperliquid API is used for nothing live. At most, it serves a one-time history backfill (§5.6).

## Interim: public feed

On 2026-10-29 Micky approved starting on Hyperliquid's public API, because a node box (about $735 a month, §6.3) costs too much for now. Until the node is up, this is what runs:

- `tr-gateway` (`server/`) with `HlPublicProvider`, which implements the `Provider` trait (§3.1) over `wss://api.hyperliquid.xyz/ws` and the public `info` endpoint. Clients speak `tr-proto` (§4) and can't tell providers apart, so `HlNodeProvider` replaces it later with no client or protocol change.
- One set of upstream connections serves every client. Mids for the core dex and for every HIP-3 dex with listed markets stay subscribed. The gateway subscribes a market's book, trades and candles only while a client watches it, and drops them 20 s after the last one leaves.
- The gateway keeps its own budgets under Hyperliquid's per-IP limits [doc: RL]: at most 8 of 10 connections, 950 of 1000 subscriptions, 1800 of 2000 sent messages a minute and 1100 of 1200 REST weight a minute (defaults 3, 900, 1000 and 800). It refuses a client subscription past the budget, and it never rotates IPs or uses proxies [doc: TOU §3.1.8].
- Hyperliquid closes the whole websocket when asked for a coin it doesn't list, with no error message [observed 2026-10-29]. The gateway only subscribes coins that have a live mid, and keeps the permanent mids on a connection of their own.
- History comes from `candleSnapshot`, so it stops at the most recent 5000 bars per interval [doc: INFO]. For 1m bars that is about 3.5 days. Node-built candles (§5.3) lift the cap.
- It makes read-only public calls only. No `exchange` endpoint, no user subscriptions, no orders.
- The desktop opts in with `TR_SERVER_URL` (§4.6). Crypto keys such as `BTCUSD Curncy` then come from Hyperliquid core perps, and the order book pane names the source. Everything else is unchanged.
- The web build uses the server for the same crypto markets. `scripts/build-web.sh` builds in `wss://terminal-gateway.style.dev/ws` unless `TR_SERVER_URL` says otherwise. While the server is unreachable, the page falls back to Binance and Coinbase as before.

How facts are marked:

- **[doc: …]** is a fact from official documentation. Sources are listed in [References](#references).
- **[observed]** is a fact from read-only calls I made against `api.hyperliquid.xyz` on 2026-10-29 around 17:35 UTC, or from reading the public `order_book_server` source. These are snapshots, not guarantees.
- **[unverified]** is an assumption or estimate that nobody has checked yet. It needs confirming before we build on it.
- **[target]** is a goal we set for ourselves. Nobody has measured it yet.

---

## 1. Goals and non-goals

### Goals

1. **Our own node, our own systems.** We run Hyperliquid non-validator nodes in Tokyo (§6). Our `tr-ingest` builds every order book, trade tape, candle and price from the node's outputs (§5). There are no live dependencies on `api.hyperliquid.xyz`, so its rate limits don't apply to live data.
2. **One source, many clients.** Ingest computes everything once. The gateway fans it out, so cost scales with instruments, not with clients (§5.9).
3. **The same data everywhere.** Desktop, web (WASM) and mobile see the same quotes, bars, books and user documents, with live updates.
4. **Hyperliquid first:** core perps, spot, and HIP-3 builder dexes (trade.xyz `xyz:` stocks, indices, FX and commodities, and the other dexes).
5. **A provider interface clean enough** that a licensed equities and options vendor (for example Intrinio, Twelve Data or Massive) slots in later with no protocol change. Each provider also carries a display-rights policy (§3.1).
6. **User sync:** accounts, layouts, watchlists, drawings, preferences and the demo (paper) portfolio, with explicit conflict rules.
7. **The web build gets real data.** This removes the `TODO(web proxy)` stubs.
8. **Candles at every interval with no history cap,** built from trades on our node (§5.3).
9. **The terminal stays snappy.** The protocol is binary and conflated, and the UI thread never waits on the network. This keeps the existing worker model in `app/src/worker.rs`.

### Non-goals (for this design)

- **No trading through the server.** The server is data only. It never signs, routes or places orders on Hyperliquid or anywhere else. The SnapTrade order paths in `crates/broker/src/trading.rs` stay client-side on the desktop, with the user's local credentials. They are unchanged and out of scope (§9). The reasons are the legal flag in §13 (Hyperliquid's Terms of Use) and Micky's steer. The demo account is paper trading against our own quotes and never touches a venue.
- No licensed stock or options data yet. Only the interface for it is in scope (§3.1).
- No public API for third parties. The protocol is for our own clients.
- No running of a Hyperliquid **validator**. We run non-validators only (§6).
- No live use of the public Hyperliquid API (the one-time backfill in §5.6 is the only exception).
- No multi-region active-active for v1.

---

## 2. Current data layer (what we map onto)

| Area | Today | Code |
|---|---|---|
| Provider trait | `MarketData` (async): `history`, `quotes`, `live`, `search`, `news`, `options`, `order_book`, `yield_curves`. Wrapped in `Fallback`, which serves sample data on error. | `crates/data/src/lib.rs` |
| Keys | Bloomberg-style market keys from `Security { ticker, class }`: `AAPL US Equity`, `BTCUSD Curncy`, `SPX Index`, `CL1 Comdty`, `US10Y Govt`. `AssetClass` has Equity, Etf, Index, Fx, Crypto, Govt and Cmdty. The yellow keys are Equity, Index, Curncy, Govt and Comdty. | `crates/core/src/security.rs`, `catalog.rs` |
| Data types | `Bar {ts, open, high, low, close, volume}` (ts is the open time in Unix seconds), `Series`, `Timeframe` (M1 to MN1), `Quote {symbol,last,prev_close,ts,spark}`, `OrderBook {bids,asks,source,simulated}`, `OptionChain`, `YieldCurves`, `NewsItem`, `SearchHit`. All of them derive serde. | `crates/core/src/bar.rs`, `timeframe.rs`, `crates/data/src/lib.rs` |
| Live data | `LiveFeed = mpsc::Receiver<Bar>` holds the latest state of the forming bar. Crypto comes from Binance kline websockets, with Coinbase as the fallback. | `crates/data/src/crypto.rs`, `ws.rs` |
| Runtime | tokio on the desktop. On wasm32, `rt::spawn` runs tasks on the browser event loop. | `crates/data/src/rt.rs` |
| Worker | The UI sends `Request` (History, MoreHistory, Quotes, OrderBook, Options, News, Search, Congress queries, Portfolio, Broker*) and drains `Response`, so it never blocks. | `app/src/worker.rs` |
| Bar cache | SQLite `BarCache` (desktop only). | `crates/data/src/cache.rs` |
| Congress | Senate eFD and House Clerk ingest into SQLite, refreshed every 30 minutes (`CONGRESS_REFRESH_MIN`). The web build gets an empty stub. | `crates/congress/` |
| Brokerage | SnapTrade, read-only. The partner keys and the per-user secret live in the Keychain or a 0600 file. | `crates/broker/src/snaptrade.rs`, `secrets.rs` |
| Demo | `DemoBook` starts with $100k (`STARTING_CASH`) and is saved as local JSON. Orders fill at the terminal's live quote. | `crates/broker/src/demo.rs` |
| User docs | Layouts (`LayoutDoc` JSON, which holds the panes, their drawings and the watchlist panes), prefs, themes and palette history. They are files on the desktop and `localStorage` in the browser. | `app/src/storage.rs`, `layouts.rs`, `prefs.rs` |

The plan keeps `MarketData` as the *client-side* trait. A new `RemoteMarkets` implementation forwards each call to the server (§4.6), so the panes, the worker and the chart code don't change. On the server, a new, richer `Provider` trait (§3.1) sits behind the ingest adapters.

---

## 3. Components

```
  Tokyo (node hosts, x2 for failover)                       Gateway host(s)
 +----------------------------------------------+          +----------------------------------------------+
 | hl-visor -> hl-node (non-validator)          |          | tr-gateway                                   |
 |   ~/hl/data/node_fills_by_block/...          |          |   hot store (watch/broadcast), refcounts     |
 |   ~/hl/data/node_order_statuses_by_block/... | internal |   auth, subs, conflation, backpressure       |--> desktop
 |   ~/hl/data/node_raw_book_diffs_by_block/... |  stream  |   user sync (docs, drawings, demo book)      |--> web (WASM)
 |   ~/hl/data/hip3_oracle_updates/..., misc_.. |--------->|   Congress, SnapTrade (read-only), others    |--> mobile
 |   localhost:3001/info (subset, fileSnapshot) | tr-proto |   persistence: Postgres/SQLite               |
 | tr-ingest (same host)                        | over     +----------------------------------------------+
 |   file tailers -> book builder (L4->L2/BBO)  | WG/mTLS
 |   trade tape -> candle builder (all TFs)     |
 |   prices: mid, last, oracle, mark, funding   |
 |   candle store (no cap), checkpoints         |
 +----------------------------------------------+
```

`tr-ingest` has to run **on the node host**, because it reads the node's local files and its `localhost` info server. The reference order-book server is also meant to run on the same machine as the node [doc: OBS]. `tr-gateway` can share the host or sit on a separate small machine (§11).

### 3.1 Ingest adapters behind a `Provider` trait

```rust
/// Server-side source of market data. One instance per upstream (Hyperliquid, a licensed
/// equities vendor, ...). The registry asks each provider what it can serve.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;                       // "hyperliquid", "intrinio", ...
    fn capabilities(&self) -> Capabilities;           // channels × asset classes it serves
    /// Display rights for data from this provider. Enforced by the gateway, not the client.
    fn display_policy(&self) -> DisplayPolicy;
    /// Instruments this provider lists right now (refreshed periodically; delistings appear here).
    async fn instruments(&self) -> Result<Vec<InstrumentDef>>;
    /// Historical bars, newest `limit` before `end` (same contract as MarketData::history).
    async fn history(&self, inst: &InstrumentId, tf: Timeframe, end: Option<i64>, limit: usize)
        -> Result<Vec<Bar>>;
    /// Start an upstream stream; events go to `sink` until the handle is dropped.
    async fn subscribe(&self, ch: Channel, inst: &InstrumentId, sink: UpstreamSink)
        -> Result<UpstreamHandle>;
}

pub struct DisplayPolicy {
    /// May we show this data to the general public (any signed-in or anonymous user)?
    pub public_display: bool,
    /// If not public: which entitlement unlocks it (e.g. a licensed seat, a BYO key, staff).
    pub entitlement: Option<Entitlement>,
    /// Required delay before display to non-entitled users (licensed vendors often have one).
    pub delay: Option<Duration>,
    /// Attribution string the UI must show ("Data: Hyperliquid", "trade.xyz oracle", ...).
    pub attribution: &'static str,
}
```

Rules:

- **Secondary crypto exchanges are off by default for public display.** Coinbase and OKX ban public display without written consent, and Kraken requires permission (provider research from the parent agent, which I have not re-checked here). Binance's terms are not reviewed [unverified]. So the existing `CryptoProvider` (Binance and Coinbase) moves server-side with `public_display: false`. It is only served to entitled users (staff, for example) until each venue's rights are confirmed in writing.
- Yahoo Finance and the RSS news sources get `public_display: false` until reviewed [unverified]. Treasury.gov is US government data and is *likely* fine [unverified].
- The gateway checks `DisplayPolicy` on every subscribe (§4). A client can't opt itself in.
- Licensed stocks and options (Intrinio, Twelve Data, Massive or another vendor) arrive later as one more `Provider`. Their entitlement and delay map straight onto `DisplayPolicy`.

Adapters at launch:

| Adapter | Source | Notes |
|---|---|---|
| `HlNodeProvider` | Our node's output files plus its local info server (`localhost:3001/info`) | §5 and §6. The only live Hyperliquid source. |
| `HlBackfill` (one-time job) | S3 node archives and/or public `candleSnapshot` | §5.6. Not a live provider. |
| `CongressProvider` | Senate eFD and House Clerk | Reuses `tr-congress` unchanged (§9) |
| `SnapTradeProvider` | SnapTrade, read-only server mode | Per-user and private, so it is not a market-data topic. Desktop local mode is unchanged (§9). |
| `CryptoProvider` (existing) | Binance and Coinbase | Off for public display |
| `TreasuryProvider` (existing) | Treasury.gov, with FRED as fallback | Yield curves |

### 3.2 Normalizer

This converts provider payloads into the shared types in `tr-core` and `tr-data`, plus a few new fields:

- **Numbers.** Node outputs carry prices and sizes as decimal **strings** (for example `"px": "51.367"`) [doc: L1]. The public API does the same, including websocket candles, which the docs type as numbers [observed]. Parsers accept either. The normalizer parses to `f64` for the existing types. It keeps the original string only where exact decimals matter: tick or lot rounding in the demo book.
- **Time.** Node events carry `time` as an ISO-like string (for example `"2024-07-26T08:26:25.899"`) [doc: L1]. Batched output adds `block_time` and `block_number` [doc: NODE]. We treat these as UTC [unverified: no zone suffix in the examples]. `Bar.ts` is the bar's open time in seconds, as today.
- **Intervals.** We build every app `Timeframe` (M1, M5, M15, M30, H1, H4, D1, W1, MN1) ourselves, plus any extra interval we choose (§5.3). Hyperliquid's own intervals (`1m` to `1M` [doc: INFO]) only matter for the one-time backfill.
- **Books.** Our L2 aggregation (§5.2) becomes `OrderBook { bids, asks, source: "Hyperliquid (own node)", simulated: false }`. L4 (per-order) views get a new type.
- **Price basis.** This is new and required for the perp-as-proxy mapping (§7). Every quote carries `basis: Mid | Mark | Oracle | Last`, `venue`, `instrument_kind: Spot | Perp | Equity | …`, plus `session: Regular | Extended | Closed | Internal24x7` when the provider knows it.

### 3.3 In-memory hot store with fan-out

- The store is a **topic table** keyed by `Topic` (for example `Quote(MarketKey)`, `Bars(MarketKey, Timeframe)`, `Book(MarketKey, depth, group)`, `Trades(MarketKey)`, `Ctx(MarketKey)`, `Congress(Latest)`, `Doc(UserId, DocId)`).
- **State topics** (quote, book, ctx) use `tokio::sync::watch` style "latest value wins". A slow reader skips intermediate values, which conflates naturally.
- **Bars are a hybrid, not a pure state topic.** Updates to the *forming* bar may be conflated, but only within the same bar `ts`. When the bar `ts` advances, the server first emits a non-conflatable `BarClosed { bar }` event with the previous bar's final OHLCV, then the new forming bar. `BarClosed` goes through the event ring like trades, so a slow client can never skip a closed bar's final values. If it overflows, the client gets `Resync` and a fresh history snapshot (§4.3).
- **Event topics** (trades, congress alerts, document ops) use a bounded `broadcast` ring with a per-topic sequence number. A reader that falls behind the ring gets `Resync` (§4.3).
- Each topic keeps a **refcount** of client subscribers. Ingest maintains *every* book, tape and candle regardless of demand, because it has to apply every diff to stay correct. The gateway's refcount only decides which per-coin topics it materialises and serialises for clients. Topics are dropped after a **linger** period (start at 60 s [target]) once the count returns to 0.
- Snapshots are served from memory, so a new client gets the current book or quote immediately.

### 3.4 Persistence (candles and history)

- **Candles:** built by ingest from node trades (§5.3) and stored with **no cap**. We keep 1m bars forever, plus materialised higher intervals. Each bar records its provenance: `node:<host>`, `backfill:s3`, `backfill:api` or `repaired`.
- **Storage:** v1 uses SQLite (WAL) on the node host's NVMe for the candle store, reusing the `BarCache` schema ideas, and replicates closed bars to the gateway's database. Postgres or Timescale can come once there are several gateway instances. Size: ~1,000 active instruments × 1,440 1m bars/day ≈ 1.4M rows/day, roughly **70–150 MB/day or 25–55 GB/year** at 50–100 bytes per row [unverified, rough; this is an upper bound because quiet minutes write no row].
- **Trade tape:** we keep the deduplicated trades (§5.3) for 30 days [target] for the tape and for rebuilding bars. Raw node files are rotated away (§6.4).
- **Also persisted** (gateway side): congress trades, user documents, demo books and audit logs (§8).

### 3.5 Client gateway

- **Transport:** WebSocket over TLS (axum with tokio-tungstenite on the server). The web client uses the browser `WebSocket` that `crates/data/src/ws.rs` already wraps.
- **Per-connection task:** auth handshake, a subscription set, a bounded outbound queue, a conflation timer and heartbeats.
- **Enforcement:** auth, per-user limits (subscriptions per connection, messages per second), and `DisplayPolicy`.
- **HTTP endpoints:** `/healthz`, `/readyz` and `/metrics` (internal), plus OAuth callback routes (§8).

---

## 4. Client protocol

### 4.1 Shared crate: `crates/proto` (`tr-proto`)

The protocol is one crate, used by the server, the desktop app and the WASM build. It depends only on `serde` and `tr-core`, so it compiles for `wasm32-unknown-unknown` (CI checks this). Every message type lives here, and the server and clients cannot drift apart because they compile the same definitions.

### 4.2 Message schema

```rust
pub const PROTOCOL_VERSION: u16 = 1;

/// Client → server
pub enum ClientMsg {
    Hello { version: u16, encoding: Encoding, client: ClientInfo, token: Option<String>, resume: Option<ResumeToken> },
    Subscribe { id: SubId, topic: TopicReq },            // client-chosen id, unique per connection
    Unsubscribe { id: SubId },
    Resync { id: SubId },                                 // client detected a gap; ask for a fresh snapshot
    Request { id: ReqId, req: Rpc },                      // one-shot: History{key,tf,end,limit}, Search, Options…
    DocOp { id: ReqId, op_id: OpId, doc: DocId, base_rev: u64, op: DocOperation },   // user-sync writes (§8); op_id is a client UUID, stable across retries
    Ping { t: u64 },
}

/// Server → client
pub enum ServerMsg {
    Welcome { version: u16, session: SessionId, user: Option<UserInfo>, server_time_ms: i64, limits: Limits },
    Snapshot { id: SubId, seq: u64, data: TopicData },   // full state; resets the subscription's seq
    Delta { id: SubId, seq: u64, data: TopicDelta },     // seq = previous + 1
    Resync { id: SubId, reason: ResyncReason },          // server dropped deltas; a Snapshot follows
    Reply { id: ReqId, result: Result<RpcResult, ApiError> },
    DocAck { id: ReqId, doc: DocId, rev: u64 } | DocConflict { id: ReqId, doc: DocId, current: DocSnapshot },
    Status { id: SubId, status: TopicStatus },            // Stale{since}, Delisted, Unavailable{reason}, DisplayRestricted
    Pong { t: u64, server_time_ms: i64 },
    Error { code: ErrorCode, message: String },
}

pub enum TopicReq {
    Quotes { keys: Vec<MarketKey>, max_hz: u8 },          // watchlists / tickers: one sub, many keys
    Bars { key: MarketKey, tf: Timeframe, history: usize },// snapshot = last `history` bars, deltas = forming bar / new bar
    Book { key: MarketKey, depth: u8, group: Option<f64> },
    Trades { key: MarketKey },
    Ctx { key: MarketKey },                               // funding, OI, mark/oracle (perps), supply (spot)
    Congress(CongressTopic), Portfolio, Docs { kinds: Vec<DocKind> }, Alerts,
}
```

### 4.3 Snapshot + delta, sequencing and resync

- Every subscription has its own `seq`. A `Snapshot` sets it, and each `Delta` increments it by 1.
- **The client checks continuity.** On a gap, or on a `Delta` before its `Snapshot`, the client sends `Resync`. The server answers with a new `Snapshot`.
- **The server sends `Resync` itself** when an event topic's ring overflowed for that client (§5.8), or after ingest recovers from a gap or a node failover (§5.5, §6.6), because the server's own state may have jumped.
- **For state topics a delta is just the new value** (a quote or a book), so a lost delta is harmless and those topics never need a resync on overflow. Bars deltas are `Forming(bar)` (conflatable within one `ts`) or `Closed(bar)` (never conflated, always delivered before the first `Forming` of the next `ts`). The client applies `Closed` by *replacing* the bar with that `ts`, which repairs any partial bar it had. Today's `Series::apply_live` only appends a newer bar, so `RemoteMarkets` must apply `Closed` with a replace before forwarding to the existing `LiveFeed` consumer. A test in phase 5 covers a slow reader across a boundary.
- **Reconnect:** `Hello.resume` holds the previous session's resume token. The server re-attaches the subscriptions and sends a fresh `Snapshot` for each one. Clients never replay old deltas.

### 4.4 Encoding

| Option | Pros | Cons |
|---|---|---|
| **postcard** (serde, compact varint) | Smallest frames. no_std-friendly, so it builds fine for WASM. Same serde derives as today. | Not self-describing: fields can't be added to an existing struct without a version bump. |
| bincode 2 | Fast and mature | Larger than postcard for small numbers. Same evolution caveat. |
| JSON (serde_json) | Debuggable, browser devtools-friendly | 3–5× larger, and parse cost on the WASM main thread [unverified ratio] |
| MessagePack (rmp-serde) | Self-describing and compact-ish | Middle ground. Another dependency. |

**Recommendation: postcard over binary WebSocket frames for every client, with JSON as a negotiated debug encoding** (`Hello.encoding`). The web client is Rust compiled to WASM, so there is *no JavaScript consumer*: the usual "JSON for the web" argument doesn't apply here. Evolution rules: append-only enum variants, a new message variant (never a changed struct) for new fields, and `PROTOCOL_VERSION` checked in `Hello`/`Welcome`. The server supports N and N−1.

Compression: the browser and tungstenite support for permessage-deflate needs checking [unverified]. postcard plus conflation may make it unnecessary. We measure in phase 6.

### 4.5 Latency targets

| Path | Target |
|---|---|
| Node writes a block's events → ingest has applied them (book, tape, bar) | **< 5 ms / < 20 ms** p50/p99 [target], using `--disable-output-file-buffering` [doc: LAT] |
| Ingest event → written to client sockets by the gateway (same region, p50 / p99) | **< 5 ms / < 25 ms** [target] |
| Subscribe → Snapshot for a hot topic (already in memory) | **< 50 ms** plus RTT [target] |
| Subscribe → Snapshot for a cold topic | **< 100 ms** plus RTT [target]. Ingest already holds every book. |
| Quote fan-out cadence to UI | Default 4 Hz per watchlist. The visible chart's bar and book stream at block speed. [target] |

For comparison, the public `l2Book` feed is pushed "on each block that is at least 0.5 since last push" [doc: WS-SUB]. Our own builder emits on every block. The docs say node outputs are "faster and more granular data than the API" [doc: LAT], but give no numbers [unverified until we measure]. For North American users, the Tokyo-to-client network hop dominates end to end [unverified RTT; to measure].

### 4.6 Client integration

- **`tr-data::remote::RemoteMarkets`** implements `MarketData` over one shared connection. `history` maps to an RPC, `quotes` to a short-lived or shared `Quotes` subscription, and `live` to a `Bars` subscription bridged into the existing `mpsc::Receiver<Bar>`.
- **Selection:** `TR_SERVER_URL` (or a settings toggle) picks `RemoteMarkets`. The direct providers stay as the offline and dev fallback (`TR_PROVIDER=sample` keeps working).
- **Web build:** `for_browser()` puts `RemoteMarkets` in front of the browser's own sources when the page was built with a server URL (`TR_SERVER_URL`, read by `scripts/build-web.sh`; production uses `wss://terminal-gateway.style.dev/ws`). The client runs on the page's event loop over the browser's `WebSocket`, with postcard in binary frames like the desktop. `?server=ws://localhost:8787/ws` points a page at a server on the same machine, and `?server=off` turns the server off. Crypto is the only asset class the server serves so far, so the `TODO(web proxy)` stubs in `crates/data/src/web.rs` and `crates/congress/src/db_web.rs` stay until it serves the rest.

---

## 5. Ingest from our node

Everything live comes from our own non-validator node (§6) through `tr-ingest`, which runs on the node host.

### 5.1 What the node writes, and the flags we run

`hl-visor run-non-validator` writes to `~/hl/data`. The output we use is opt-in via flags [doc: NODE]:

| Flag | Output | We use it for |
|---|---|---|
| `--write-fills` | `node_fills` (API fills format). It "overrides `--write-trades` if both are set" and also writes TWAP statuses. HIP-3 fills include `deployerFee`. | The trade tape and candles (§5.3). Fees and user fields are dropped before anything leaves ingest. |
| `--write-order-statuses` | `node_order_statuses`, every order status update, "a substantial amount of data" | L4 order metadata (tif, cloid, timestamp) and order lifecycle (§5.2) |
| `--write-raw-book-diffs` | `node_raw_book_diffs`: per-order `new {sz}`, `update {origSz, newSz}` or `remove` with `user, oid, coin, side, px` [doc: L1] | Applying changes to the L4 book (§5.2) |
| `--write-hip3-oracle-updates` | `hip3_oracle_updates`: every HIP-3 deployer `setOracle` | HIP-3 oracle and mark inputs (§5.4) |
| `--write-misc-events` | `misc_events`, including a `Funding` event `{coin, usdc, szi, fundingRate, nSamples}` and ledger updates [doc: L1] | Realised funding (§5.4) |
| `--batch-by-block` | Writes `*_by_block` files, one line per block: `{local_time, block_time, block_number, events}` | Block-aligned processing, gap detection, deduplication across two nodes |
| `--disable-output-file-buffering` | Flushes each line as soon as it's written | Latency [doc: LAT] |
| `--serve-info` | Local info server at `localhost:3001/info` with a subset of info requests, plus `fileSnapshot` | The registry, L4 snapshots, freshness checks, marks (§5.4) |
| `--replica-cmds-style actions-and-responses` (optional) | `replica_cmds`: every L1 transaction | Core oracle derivation, if needed (§5.4). Off unless phase 1 shows we need it, because it is large [unverified size]. |

Every output is hourly-partitioned under `{dir}/hourly/{date}/{hour}` [doc: NODE]. Ingest tails the current file with a checkpoint `(file, byte offset, last block_number)`, written after the derived state has been persisted. So a restart resumes exactly where it stopped. The docs say batching by block costs "a few milliseconds" against the streaming mode [doc: OBS, LAT]. v1 keeps `--batch-by-block` for correctness, and we revisit it once we measure.

### 5.2 Order books: can we rebuild full L2 and L4? Verdict: yes, with caveats

**Yes for core perps.** The node's own docs describe this exact use:
- L4 snapshots hold "the entire order book with full information about the orders for each level. This can be used as a checkpoint upon which the order statuses stream may be applied, allowing users to stream an L4 book in realtime" [doc: L1].
- The latency guide says node outputs let us "construct the book" locally, with "faster and more granular data than the API" [doc: LAT].
- The reference [`order_book_server`](https://github.com/hyperliquid-dex/order_book_server) does it in Rust. It serves `l2Book` (up to 100 levels), `trades` and a new `l4Book` over a websocket, built from the `*_by_block` files for fills, order statuses and raw book diffs [doc: OBS].

**How the rebuild works** (the reference server's approach, which we copy) [doc: OBS; observed in source]:
1. Checkpoint: POST `{"type":"fileSnapshot","request":{"type":"l4Snapshots","includeUsers":true,"includeTriggerOrders":false},"outPath":…,"includeHeightInOutput":true}` to the local info server. It writes the current L4 book for every coin, at a known height, and "uses the latest state instead of an older snapshot file" [doc: NODE]. As an offline alternative, `hl-node compute-l4-snapshots` can build one from a `periodic_abci_states` file (written every 10,000 blocks) [doc: NODE].
2. Apply every raw book diff and order status from the `*_by_block` files with `block_number` above the snapshot height, in order. `new` inserts an order (`oid`, side, `px`, size) at the back of its price level. `update` changes its size. `remove` deletes it. Order statuses give the metadata the diffs lack (timestamp, tif, cloid, order type).
3. L2 is an aggregation of L4: Σ size and order count `n` per price. We do any number of levels and our own price grouping, without the API's 20-level cap [doc: INFO]. BBO and mid come out of L2.
4. Self-check: the reference server periodically takes a fresh snapshot, compares it with its in-memory book, and exits on a mismatch. It also exits if no events arrive for 5 s [observed in source]. We do the same, but rebuild that coin instead of exiting.

**Caveats and gaps (stated plainly):**
- **HIP-3 coverage is likely but not documented.** Nothing in the node docs says explicitly whether HIP-3 dex books appear in `node_raw_book_diffs`, `node_order_statuses` or `l4Snapshots`. The evidence points to yes:
  - the fills docs mention `deployerFee` "for HIP-3 fills" [doc: NODE];
  - HIP-3 markets trade on HyperCore's matching engine [doc: HIP3];
  - the reference server takes its coin universe from the snapshot and has no dex filter [observed in source].

  **[unverified]**. It's the phase 1 acceptance test (§12): grep a day of diffs and a snapshot for `xyz:`, `flx:` and so on, and compare one `xyz:` book against the public `l2Book` once. If HIP-3 books are missing, that is a real gap. We'd have no source for HIP-3 books except the public API, which goes against the decision. We'd come back to Micky before going further.
- **Spot:** the reference README says it "does not support spot order books", but its code has `is_spot` handling (`@…` and `PURR/USDC`) and an `ignore_spot` switch [doc: OBS; observed in source]. Spot diffs should be in the same files [unverified]. Our builder treats spot like perps and verifies it the same way.
- **Trigger orders** (stop and take-profit) aren't resting liquidity. The reference server leaves them out [doc: OBS]. Snapshots can include them with `isTrigger`, at the end of each level [doc: L1]. We leave them out of L2 too.
- **Queue position:** "ALO priority fees or other edge cases" can break naive time priority within a level [doc: LAT]. L2 is unaffected. L4 order position within a level is best-effort [unverified]. The snapshot self-check catches drift.
- **The reference code is unmaintained.** It is "not actively maintained" [doc: LAT], "not written by the core team" and provided as-is [doc: OBS]; its last commit was 2026-07-15 [observed]. So we write our own `tr-book` crate, using it as a reference, and we test it against recorded node files.

### 5.3 Trades and candles at every interval, no cap

- **Tape:** with `--write-fills`, each match shows up as two fills, one per side. Ingest dedupes them into one public trade, keyed by `(coin, tid)` [unverified that node fills carry `tid`; the trades schema carries `hash` plus buyer and seller `oid`s, which works as a fallback key, doc: L1]. The aggressor side comes from the taker fill.
- **1m bars** are built from the deduplicated trades by UTC minute: OHLC from trade prices, volume Σ sz (base), notional Σ px·sz, and trade count `n`. A bar closes when the first block with a `block_time` at or after the minute boundary has been applied. That makes closes deterministic and identical on both nodes.
- **Higher intervals** are aggregated from 1m. OHLCV aggregation is exact, so 5m, 15m, 1h, 4h, 1d, 1w and 1M (and any custom interval) are consistent by construction. Weeks start Monday 00:00 UTC and months are calendar months [target; Hyperliquid's own boundary convention is undocumented, unverified].
- **Minutes with no trades** produce no bar row. Clients render the gap, which matches how sparse HIP-3 markets look today.
- **No cap:** every bar since our node started is kept (§3.4). This replaces the public API's "most recent 5000 candles" limit [doc: INFO].
- Live bars reach clients as `Forming` and `Closed` (§4.3).

### 5.4 Mids, marks, oracle prices and funding

| Value | Where it comes from | Confidence |
|---|---|---|
| **Mid** | Our book: `(best bid + best ask) / 2`. One-sided or empty books publish no mid, and clients fall back to mark with a label. | Derived. Exact for two-sided books. |
| **Last trade** | Our tape (§5.3) | Exact |
| **HIP-3 oracle and mark inputs** | `--write-hip3-oracle-updates` records each deployer `setOracle`, which carries `oraclePxs`, `markPxs` and `externalPerpPxs` per coin [doc: NODE, HIP3-ACT]. The oracle price is used as-is. | Exact for the oracle |
| **HIP-3 mark** | The protocol forms the final mark from the deployer's `markPxs` with documented rules: the mark is clamped to move at most 1% per update, and stale marks fall back to the local mark after 10 s [doc: HIP3-ACT]. We implement those rules and reconcile the result against `activeAssetData` (below). | The final combination is [unverified]. To validate in phase 1. |
| **Core perp oracle** | Validators publish oracle prices about every 3 s. Each is a weighted median of CEX spot prices (Binance 3, OKX 2, Bybit 2, Kraken, Kucoin, Gate, MEXC and Hyperliquid spot 1 each), and the final price is a stake-weighted median of the validators' submissions [doc: ORACLE]. On the node, these validator actions should be in `replica_cmds` [unverified; the action schema is undocumented]. | Derivable [unverified]. Fallback: the abci state (below). |
| **Core perp mark** | The median of (a) oracle + a 150 s EMA of (Hyperliquid mid − oracle), (b) the median of best bid, best ask and last trade, and (c) a weighted median of Binance, OKX, Bybit, Gate and MEXC perp mids [doc: MARK]. We can compute (a) and (b), but **not (c)** from node data. So we **read** mark rather than derive it. The local info server supports `activeAssetData`, which returns `markPx` per coin [doc: NODE, PERP]. Ingest polls it locally (no rate limit) every 1 s for watched coins and every 3 s for the rest [target]. Mark only updates with the oracle, about every 3 s [doc: MARK]. | [unverified that the local server returns a live `markPx`]. Fallback: a computed approximation from (a) and (b), labelled "est.". |
| **Funding (realised)** | Hourly `Funding` events in `misc_events` carry `coin` and `fundingRate` [doc: L1] | Exact |
| **Funding (predicted)** | Computed with the documented formula: the hourly rate is ⅛ of the 8 h rate, F = average premium + clamp(interest − premium, ±0.0005), interest is 0.01% per 8 h, the premium is sampled every 5 s from impact prices against the oracle, and funding is capped at 4% per hour [doc: FUNDING]. Impact prices come from our own book. HIP-3 dexes can set their own multipliers and interest rates [doc: HIP3-ACT] [unverified where these show up in node output]. | Derived |
| **Open interest, 24h volume, prev-day price** | Volume and the previous day's price come from our candles. Open interest isn't in the documented outputs. We'd derive it from the `periodic_abci_states` state, translated with `translate-abci-state` [doc: NODE], at that file's cadence, or track it live from fills (`start_pos`) [unverified; later]. | Partial |

Reconciliation: every 10,000 blocks, a job translates the `periodic_abci_states` file and checks our oracle, mark, funding and open interest against it [unverified that every field is present]. Mismatches raise an alert.

### 5.5 Catch-up, gaps and failover inside ingest

- **Restart:** ingest resumes from its checkpoint (§5.1). Books are never trusted across a restart: it takes a fresh L4 snapshot (§5.2) and replays diffs after that height.
- **Gaps:** `block_number` must increase by exactly 1. A gap or a stall (no new block for 5 s [target], the same threshold the reference server uses) marks every topic `Stale`. Ingest re-snapshots the books, and asks the peer node's ingest for the missing blocks' trades so bars are complete (§6.6). If both nodes miss a range, a repair job fills the trades later from the S3 archive (§5.6), and the affected bars are flagged `repaired`.
- **Node restart:** see §6.5.

### 5.6 The one-time history backfill (the only public data use)

Node-built bars cover everything from the first full minute after our node is live. History before that is a one-time job, run once and then never again:

1. **Primary: S3 node archives, rebuilt into trades and then candles with the same code as live.** The bucket `hl-mainnet-node-data` holds `node_fills_by_block` (written with `--write-fills --batch-by-block`), older `node_fills` (API format) and `node_trades` (a different, older format), plus `misc_events_by_block` (funding and more), `explorer_blocks` and `replica_cmds` [doc: HIST]. The archives are **requester-pays**: we pay the S3 transfer, which needs our own AWS account [doc: HIST]. The **start date and total size are not documented** [unverified]. Listing the bucket needs AWS credentials, which I don't have, so phase 1 measures them first. This route gives true 1m (and finer) history and is consistent with live bars.
2. **Secondary: one public `candleSnapshot` sweep**, for instruments or ranges the archive lacks. It returns only the most recent 5000 candles per interval [doc: INFO]: 1m ≈ 3.5 days, 1h ≈ 208 days, 1d back to listing (BTC 1d starts 2020-08-19) [observed]. The weight math for a one-off sweep: ~900 instruments (552 perp entries including delisted, plus 347 spot pairs [observed, §5.7]) × 5 intervals (1m, 15m, 1h, 4h, 1d) × ~104 weight (20 + 5000/60) ≈ 468k weight. That is ≈ 7.8 hours at a 1000/min self-limit, under the 1200/min IP limit [doc: RL]. It runs once, from one IP, with no evasion of limits.
3. **Optional: `hyperliquid-archive/asset_ctxs/[date].csv.lz4`** for daily historical funding, open interest and mark context. It's uploaded about monthly, "with no guarantee", and has no candles or spot asset data [doc: HIST].

**Stitching:** for each `(instrument, interval)`, node-built bars win for every bar at or after node start. Backfill bars fill the earlier range, and each bar keeps its provenance. Where S3-rebuilt and `candleSnapshot` bars overlap, we compare them as a test of our aggregation. Any differences are logged, and the S3 bars win.

### 5.7 Addressing (HIP-3, spot and outcomes)

- `perpDexs` returns `[null, {name, fullName, deployer, oracleUpdater, feeRecipient, assetToStreamingOiCap, …}, …]`. `null` is the first (core) perp dex [doc: PERP]. I observed 11 entries: `null`, `xyz` (XYZ), `flx` (Felix Exchange), `vntl` (Ventuals), `hyna` (HyENA), `km` (Markets by Kinetiq), `abcd` (ABCDEx), `cash` (dreamcash), `para` (Paragon), `mkts` (Markets By Kinetiq) and `io` (EntropyIO). The objects also carried `subDeployers`, `assetToFundingMultiplier`, `assetToFundingInterestRate` and `assetToFundingClamp`, which are not in the doc example [observed].
- `meta`, `metaAndAssetCtxs`, `allMids`, `clearinghouseState` and others take `"dex": "<name>"`. The empty string means the first perp dex [doc: PERP, INFO]. `allPerpMetas` returns every dex at once [doc: PERP].
- HIP-3 coin names are always `{dex}:{coin}`, for example `xyz:TSLA` [doc: ASSET, INFO]. `candleSnapshot` needs the prefix: "For HIP-3, you need to prefix with the dex name, e.g. "xyz:XYZ100"" [doc: INFO]. The WS `allMids` with `dex:"xyz"` returned only `xyz:`-prefixed keys plus a `dex` field [observed].
- Spot coins are `PURR/USDC` or `@{index}` (for example, HYPE is `@107`). "Some assets may be remapped on user interfaces", for example BTC/USDC in the app is UBTC/USDC on HyperCore [doc: INFO].
- Outcome (HIP-4) markets show up as `#<encoding>` keys [doc: ASSET]. I observed 762 `#` keys and 401 `@` keys among the 1399 keys of the default `allMids` [observed]. **Outcomes are out of scope** and are filtered at ingest. They also carry separate legal restrictions [doc: TOU §1.7].
- Snapshot on 2026-10-29 [observed]: the core `meta` had 235 entries, 56 flagged `isDelisted`. The `xyz` `meta` had 141 entries, 21 flagged `isDelisted`. `spotMeta` had 347 pairs and 511 tokens. `allPerpMetas` listed per-dex universe sizes of core 235, `xyz` 141, `para` 39, `hyna` 25, `mkts` 24, `km` 23, `cash` 17, `flx` 16, `io` 16, `vntl` 15 and `abcd` 1, for 552 perp entries in total.
- **In our system** the registry comes from the node's local info server, which supports `perpDexs`, `meta` and `spotMeta` [doc: NODE]. The public-API observations above are from research and are only for scale.

### 5.8 Backpressure

- Each client has a **bounded outbound queue** (start at 256 frames or 1 MB [target]).
- **State topics** are conflated per client. On each flush tick (or immediately, for the active chart), the server writes the latest value of every dirty topic, so a slow client gets fewer updates but never stale ones.
- **Event topics:** if a client's cursor falls behind the ring, the server drops that subscription's backlog and sends `Resync` plus a `Snapshot`. For trades, the snapshot is the last N trades.
- If the queue stays full for more than 10 s [target], the server closes the connection with a "slow consumer" code, and the client reconnects with `resume`.
- **Ingest is never blocked by a client.** The node-file tailers only write into the hot store. Fan-out runs in per-connection tasks.

### 5.9 Scale without Hyperliquid rate limits

- Ingest work is proportional to market activity (all coins, every block), not to clients. The gateway's work is proportional to client subscriptions. Neither touches a Hyperliquid rate limit.
- The public API limits (1200 weight/min, 1000 WS subscriptions, 10 connections per IP [doc: RL]) apply only to the one-time backfill (§5.6). That is the sanctioned way to get unlimited data: the ToU forbids bots that "bypass rate limits" [doc: TOU §3.1.8], and a node avoids the question entirely.
- The internal ingest-to-gateway stream carries per-block batches of book changes, trades, bars and prices for every coin. Its bandwidth is unmeasured [unverified; phase 2 measures it]. Gateways can subscribe only to the topics their clients use.

---

## 6. Node operations

### 6.1 Hosting choice and region

- **Region: Tokyo.** The README says "for lowest latency, run the node in Tokyo, Japan" [doc: NODE], and the Foundation's own node is in AWS `apne1-az1` [doc: FND].
- **We are likely ineligible for the Foundation node** (it requires 10k staked HYPE and Tier 1 maker volume, and excludes "certain activities involving the U.S. or Ontario") [doc: FND]. So we run our own.
- **Provider:** single-tenant bare metal in Tokyo from two different providers, so a single provider outage doesn't take out both (§6.6). The comparison, prices and recommendation are in §6.3: Vultr for node A and Cherry Servers for node B.
- **Ruled out:**
  - Freestyle (freestyle.sh): VMs too small (64 GiB / 256 GB max), and no raw inbound TCP for gossip [doc: FREESTYLE].
  - OVHcloud: no Japan region [doc: OVH-REGIONS].
  - Equinix Metal: discontinued [doc: EQX-EOL].
- **Peers:** mainnet requires at least one root peer in `~/override_gossip_config.json`. Community root peers are mostly in Japan, and a `gossipRootIps` info request lists recent ones [doc: NODE]. The Foundation also lists paid infra providers that offer reliable peering [doc: INFRA-PEERS]. Running "against a reliable peer" is the first latency recommendation [doc: LAT]. Peer addresses live in the host's config, never in this repo or its docs.

### 6.2 Machine spec and OS

- **Minimum (non-validator):** 16 vCPU, 128 GB RAM, 500 GB SSD [doc: NODE]. "Currently only Ubuntu 24.04 is supported" [doc: NODE].
- **For low latency:** at least 32 logical cores, 128 GB RAM and 500 MB/s disk [doc: LAT].
- **Our target per node:** bare metal with 16+ physical cores (32+ threads), 128 GB+, **≥ 2 TB local NVMe**. Disk is sized for ~1–2 days of raw output (§6.4) plus abci states and the candle store. Ubuntu 24.04 LTS. Ingest, and in v1 the gateway, share the box (§6.3, "Colocating our Rust server") [unverified until measured].
- **Network:** gossip ports 4001 and 4002 open publicly [doc: NODE]. Everything else, including the info server and ingest's internal stream, is firewalled, with the internal stream over WireGuard or mTLS. Node bandwidth is undocumented [unverified; measure in week 1].
- **Install:** `hl-visor` binaries are signed. Verify them with the published GPG key, and run `hl-visor run-non-validator` with `~/visor.json` set to `{"chain": "Mainnet"}` [doc: NODE]. Run both visor and ingest under systemd. `hl-visor` auto-updates `hl-node` [doc: NODE], so we alert on version changes and re-verify ingest parsers after each one.

### 6.3 Where to host it: providers checked, with published prices

Requirements (§6.2):
- 16+ physical cores (32+ logical) and 128 GB+ RAM;
- 500 GB+ local NVMe, ideally 2 TB;
- Ubuntu 24.04;
- **raw public TCP on 4001–4002**;
- high bandwidth;
- in or near Tokyo.

Prices are list prices, checked 2026-10-29, excluding tax. "Stock" is what the provider's inventory showed that day. None of these are quotes.

| Provider and plan (Tokyo unless noted) | Hardware | Monthly price | Bandwidth | Stock | Source |
|---|---|---|---|---|---|
| **Vultr bare metal `vbm-24c-256gb-amd`** | EPYC 7443P, 24 cores / 48 threads, 256 GB. The product page also lists 2 × 1.92 TB NVMe; the public plans API lists only the 2 × 480 GB SSDs [confirm at order]. 25 Gbps. | **$725** (hourly $0.993) | 10 TB included, then $0.01/GB | Offered in Tokyo (`nrt`) and Osaka (`itm`) | [doc: VULTR-API, VULTR-BM, VULTR-PRICE] |
| Vultr bare metal `vbm-24c-384gb-amd5` | EPYC 9255, 24c/48t, 384 GB, NVMe | $995 | 10 TB | Tokyo and Osaka | [doc: VULTR-API] |
| Vultr bare metal `vbm-8c-132gb` | E-2288G, **8 cores**, 128 GB, 2 × 1.92 TB NVMe | $350 | 10 TB | Tokyo | **Fails** the 16-core minimum [doc: VULTR-API] |
| **Cherry Servers AMD EPYC 9275F** | 24c/48t at 4.1 GHz, 384 GB, 2 × 1 TB + 2 × 2 TB NVMe, 10 Gbps, Equinix TY15 | **$874.17** | 100 TB free egress | Listed as deployable in Tokyo | [doc: CHERRY-TYO] |
| Cherry Servers AMD Ryzen 9950X | 16c/32t, 128 GB (non-ECC), 2 × 1 TB NVMe, 10 Gbps | €439 (€373 on a 12-month term) | 100 TB | **Sold out in Tokyo** | [doc: CHERRY-9950X] (third-party tracker) |
| Latitude.sh `m4.metal.medium` | EPYC 9124, 16c/32t, 128 GB, 2 × 480 GB + 2 × 1.9 TB NVMe, 2 × 10 Gbps | $522 Tokyo ($365 on a 12-month term); $456 US list | 20 TB | **Sold out** in TYO3 and TYO4 | [doc: LATITUDE, LAT-M4] (Tokyo price from a third-party tracker) |
| Latitude.sh `f4.metal.medium` | EPYC 4564P, 16c/32t at 4.5 GHz, 128 GB, same disks | $715 Tokyo ($501 on term); $555 US list | 20 TB | **Sold out** in Tokyo | [doc: LATITUDE, LAT-F4] |
| AWS `i4i.4xlarge` (on demand) | 16 vCPU (8 cores), 128 GiB, 1 × 3,750 GB local NVMe | $1.61/h ≈ **$1,175** | Egress billed separately | `ap-northeast-1` | [doc: AWS-PRICE] |
| AWS `r6i.4xlarge` + 2 TB gp3 | 16 vCPU, 128 GiB, EBS only | $1.216/h ≈ $888, plus ≈ $215 EBS ≈ **$1,100** | Egress billed separately | `ap-northeast-1` | [doc: AWS-PRICE]; EBS from earlier third-party summaries |
| OVHcloud | — | — | — | **No Japan region.** Its APAC dedicated servers are in Singapore, Sydney and Mumbai only, with metered traffic that is throttled at the quota. | [doc: OVH-REGIONS, OVH-APAC] |
| Equinix Metal | — | — | — | **Discontinued.** Sunset on 2026-06-30. | [doc: EQX-EOL] |
| Freestyle (freestyle.sh) VMs | Pro tops out at 32 vCPU / **64 GiB** / **256 GB** per VM | — | — | **Ruled out:** below the 128 GB RAM and 500 GB disk minimums, and inbound traffic only reaches a VM through the TLS/SNI edge on 443, so there's no raw TCP on 4001–4002 for gossip | [doc: FREESTYLE] |

All three bare-metal providers above are single-tenant with full root, so we open 4001–4002 and firewall the rest ourselves with nftables. Ubuntu 24.04 is in Vultr's OS list [doc: VULTR-API] and Cherry's Tokyo FAQ [doc: CHERRY-TYO]. Node bandwidth needs are undocumented [unverified]. Vultr's 10 TB is the tightest allowance, and its overage ($0.01/GB) is cheap.

**Recommendation:**
- **Node A: Vultr `vbm-24c-256gb-amd` in Tokyo, $725/month.**
  - 24 cores / 48 threads meets the 32-logical-core latency advice [doc: LAT].
  - 256 GB of RAM leaves headroom for ingest and the gateway on the same box.
  - It's orderable today.
  - Confirm the NVMe drives at order time.
- **Node B: Cherry Servers EPYC 9275F in Tokyo (Equinix TY15), $874.17/month.**
  - A different provider and building, so one provider outage doesn't take out both (§6.6).
  - 100 TB egress, and faster cores.
  - Alternatively, a second Vultr box in Osaka (`itm`): a different city, but the same provider.
- **Two-node total ≈ $1,600/month.** Budget path: one node at ≈ $725, or the 16-core Latitude and Cherry boxes ($365–522) once Tokyo stock returns.
- **AWS works but costs more for less:** 16 vCPU is 8 physical cores, and egress is extra. It mainly helps the S3 backfill (§5.6), which can run from a short-lived AWS instance instead.

**Colocating our Rust server:**
- **`tr-ingest` runs on the node box** (required: it tails local files and calls `localhost:3001`).
- **v1: `tr-gateway` on the same box as well.** The node gets most of the cores via systemd `CPUAffinity`/cgroup limits, and ingest plus the gateway get a reserved slice (for example 8 of 48 threads) [target; tune after measuring]. That makes the internal stream loopback, with no extra machine.
- **When client load grows,** move `tr-gateway` to a small box on the **same provider's private LAN** in the same city (Vultr VPC or Cherry private VLAN [doc: CHERRY-TYO]), so the ingest-to-gateway hop stays sub-millisecond and off the public internet [unverified latency; measure]. Never put the gateway across the Pacific from ingest. The data starts in Tokyo either way (§11).

**Cost summary (per month, list):**
- **One node:** ≈ **$725** (Vultr bare metal). A 16-core Latitude or Cherry box would be ≈ $365–522 when Tokyo stock returns; AWS is ≈ $1,100–1,175.
- **Two nodes (recommended, §6.6):** ≈ **$1,600** (Vultr + Cherry). The range is ≈ $1,450–2,050, depending on which pair we use.
- These are list prices, not quotes. Third-party tracker figures are labelled as such in the table.

### 6.4 Disk and log rotation (~100 GB/day)

- The node writes "around 100 GB of logs per day", which should be archived or deleted [doc: NODE]. Order statuses and raw book diffs add "a substantial amount" on top [doc: NODE]. We size for **100–200 GB/day** [unverified; measure in week 1].
- **Pruner** (a systemd timer, hourly):
  - Delete hourly output directories older than **24 h** [target], *only* when ingest's committed checkpoint is past them. The pruner never deletes a file that ingest hasn't committed.
  - Keep the last 3 `periodic_abci_states` files.
  - Keep `replica_cmds` only if we enable it, with `--replica-cmds-style recent-actions` where possible [doc: NODE].
  - Optionally compress `node_fills_by_block` to object storage (zstd), as our own archive for repairs.
- **Alerts:** disk use above 70% (warn) and 85% (page). If the disk fills, the node stops writing and ingest goes stale.

### 6.5 Monitoring and restart catch-up

- **Liveness:**
  - `applied block X` log lines show the node is live [doc: NODE]. We export the block height and blocks per second.
  - Ping the local `exchangeStatus` to compare the L1 timestamp with the local timestamp [doc: NODE]. We alert when it's behind by more than 2 s [target].
  - Compare the heights of the two nodes.
- **Crashes:** `visor_child_stderr` and `node_logs/status` [doc: NODE] are shipped to logs, with an alert on new stderr content and on restarts.
- **Restart catch-up:** after a restart, the node "may take a while as the node navigates the network to find an appropriate peer" [doc: NODE]. How long catch-up takes, and whether output files are written for the blocks it replays, isn't documented [unverified; we test it with a planned restart in phase 1]. Ingest treats the node as unusable until `exchangeStatus` is fresh, then resumes from its checkpoint (§5.5). Any block range without output is filled from the other node.

### 6.6 Failover with a second node

- **Two nodes, A and B,** in Tokyo on different providers or availability zones. Each runs its own ingest and builds its own books and bars.
- **Primary and standby:** the gateway consumes both streams, tagged with `block_number`. One is primary. The gateway switches when the primary is behind by more than 20 blocks or 2 s [target], or stops sending. On a switch, it sends `Resync` for the book topics (books are per node), and uses `block_number` to drop duplicate or out-of-order trades and bar updates.
- **Bars stay identical** on both nodes because bar closes are block-time based (§5.3). The standby's candle store doubles as a hot replica for repairing gaps on the primary.
- **Upgrades:** `hl-visor` auto-upgrades [doc: NODE]. Both nodes upgrading at once is a correlated failure we can't fully prevent [unverified whether upgrades can be staggered]. We alert on it.
- **Single node (budget fallback):** one node plus the replay and repair path (S3, §5.6). Outages then mean stale data until the node catches up, shown as `Stale` in the UI.

## 7. Symbol mapping to Bloomberg-style keys

### 7.1 Principles

1. **A perp is never silently shown as a stock.** If `AAPL US Equity` is backed by `xyz:AAPL`, every surface shows a **`PERP·xyz`** badge, the attribution "trade.xyz perp on Hyperliquid", and the price basis (`mid` or `mark`). Tooltips explain that this is a perpetual futures price, not the exchange-listed share price.
2. **Venue-native keys always work.** `xyz:AAPL <GO>`, `BTC-PERP`, `@107` and `HYPE/USDC` resolve directly.
3. **The server owns the mapping table** (a versioned config, not code), so we can fix it without a client release.

### 7.2 Model changes (in `tr-core`)

```rust
pub enum InstrumentKind { Spot, Perp, Equity, Etf, Index, Fx, Future, Govt }
pub struct VenueRef { provider: ProviderId, venue: String /* "hyperliquid", "xyz" */, symbol: String /* "xyz:AAPL" */ }

pub struct Resolution {
    pub key: MarketKey,                    // what the user typed / the pane is linked to: "AAPL US Equity"
    pub source: VenueRef,                  // what actually backs it: hyperliquid / xyz / "xyz:AAPL"
    pub kind: InstrumentKind,              // Perp
    pub proxy: bool,                       // true: the source is a different instrument than the key
    pub basis: PriceBasis,                 // Mid | Mark | Oracle | Last
    pub session: Option<SessionCalendar>,  // underlying's hours (for "underlying closed" labels)
}
```

`Security { ticker, class }` stays as it is for back-compat. `Resolution` travels alongside it in `Quote`, `Snapshot` and `SearchHit`.

### 7.3 Initial mapping table (sketch; every row needs checking against the trade.xyz spec index)

| App key | Backed by | Kind | Notes |
|---|---|---|---|
| `AAPL US Equity`, `TSLA`, `NVDA`, `MSFT`, … | `xyz:<TICKER>` | Perp (proxy) | Only for tickers present and not `isDelisted` in `meta{dex:"xyz"}` |
| `SPX Index` | `xyz:SP500` | Perp (proxy) | Whether it references the S&P 500 index exactly is [unverified] |
| `NDX Index` | `xyz:XYZ100` | Perp (proxy) | XYZ100's exact reference is [unverified] |
| `NKY Index`, `KOSPI2 Index` | `xyz:JP225`, `xyz:KR200` | Perp (proxy) | [unverified] references |
| `GC1 Comdty`, `SI1`, `HG1`, `CL1`, `CO1`, `NG1` | `xyz:GOLD`, `SILVER`, `COPPER`, `CL`, `BRENTOIL`, `NATGAS` | Perp (proxy) | The futures roll vs spot reference is [unverified] |
| `EURUSD Curncy`, `GBPUSD`, `USDJPY` | `xyz:EUR`, `xyz:GBP`, `xyz:JPY` | Perp (proxy) | **The quote direction for JPY is [unverified]**. An inversion could be needed. |
| `BTCUSD Curncy`, `ETHUSD`, … | core perp `BTC`, `ETH` (mid) | Perp | An alternative is HL spot (`UBTC/USDC`, remapped) [doc: INFO]. Default open question (§15). |
| Other dexes (`flx:`, `km:`, `para:`, …) | venue keys only | Perp | Not mapped to Bloomberg keys in v1. They overlap with xyz (for example `flx:NVDA`) [observed]. |

`perpConciseAnnotations` returns categories such as `stocks` and `commodities` for HIP-3 coins [doc: PERP; observed]. We use it to suggest new rows, never to auto-map them.

### 7.4 Off-hours labelling

trade.xyz US stock perps take external prices 24/5, Sunday 8:05 PM ET to Friday 8:00 PM ET. That spans pre-market, regular, post-market and an overnight session from Blue Ocean ATS [doc: XYZ-US]. Outside external coverage, the oracle advances through an internal EMA of order-book impact prices, with τ = 30 min [doc: XYZ-ORACLE; changelog XYZ-TAU]. Mark price is bounded to ±(1/max leverage) around a reference price, which is the last external price over weekends [doc: XYZ-BOUNDS]. So the UI shows the session state (`Regular`, `Extended`, `Overnight`, `Closed: perp-only price discovery`) next to proxy quotes. A snapshot of the gap, from `xyz:AAPL` during US hours: `markPx 349.72` vs `oraclePx 349.8` [observed].

### 7.5 Delistings and churn

The registry refreshes `perpDexs`, `meta` (per dex) and `spotMeta` from the node's local info server every minute [target]. These calls are local, so there's no rate limit [doc: NODE]. If an instrument turns `isDelisted`, or disappears, the server marks the topic `Delisted`, stops live streams, keeps its history, and notifies users who have it in watchlists or layouts. HIP-3 deployers can also halt and settle a market with `haltTrading`, which can later resume and "recycle" the asset [doc: HIP3]. That means the *same name may come back as a different contract*. History is therefore keyed by `(dex, coin, listing_epoch)`, and a relisting starts a new epoch [target].

---

## 8. User sync: accounts, documents and conflicts

### 8.1 Accounts and auth

- **Sign-in:** OAuth (Apple, Google, GitHub) plus email magic link. Desktop uses the system browser with a loopback redirect (RFC 8252 style). The web uses a same-site HTTP-only session cookie. Mobile uses the platform's auth session.
- **Tokens:** short-lived access tokens (JWT, 15 min [target]) plus refresh tokens, rotated and revocable. The WS `Hello` carries the access token. When it expires, the client re-authenticates in place without dropping subscriptions.
- **Anonymous mode:** public market data with no sync, which matches today's experience.
- **Build or buy:** a hosted identity provider (Clerk, WorkOS, Auth0 or Supabase Auth) vs a small self-hosted OAuth layer in Rust. See the open question in §15.
- **Secrets:** SnapTrade per-user secrets are encrypted at rest (envelope encryption with a KMS or age key). They are never logged and never sent back to clients.

### 8.2 Documents

Each file in `storage.rs` becomes a server document `(user_id, kind, name)` with a server-assigned, monotonically increasing `rev`:

| Kind | Today | Server model | Conflict rule |
|---|---|---|---|
| Layouts | `LayoutDoc` JSON | Whole-document, optimistic concurrency (`base_rev`) | If `base_rev` doesn't match, the server keeps both and saves the losing write as "Name (conflict, Device, time)". This mirrors how people expect files to behave and never loses work. |
| Watchlists | Watchlist panes or prefs | Ordered set of items with stable ids | Per-item ops (`Add`, `Remove`, `Move{after}`), last-writer-wins per item, with tombstones. Concurrent adds from two devices both survive. |
| Drawings | `Vec<Drawing>` inside chart panes | Stored separately per `(user, market_key)`, each drawing with a **new stable `id`** (the `Drawing` struct needs one) | Last-writer-wins per drawing (`rev` and `updated_at`). Delete wins over a concurrent edit. Drawings follow the symbol across layouts. |
| Prefs, themes, hotkeys, palette history | TOML/JSON files | Key-level merge | Last-writer-wins per key |
| Demo portfolio | `DemoBook` JSON | **Server-authoritative.** Clients send commands, and the server fills against its own hot quotes and broadcasts the book. | No merge needed: one writer. Paper only; it never routes to a venue. |

Offline behaviour: clients keep today's local files as a cache and an outbox. Each queued `DocOp` carries an `op_id` (a UUID generated once, when the edit is made, and persisted with the outbox entry). The server records `(user_id, op_id) → result (rev or conflict outcome)` **in the same transaction as the write**, and keeps it for at least 30 days [target]. On reconnect, clients replay queued `DocOp`s with their original `op_id` and `base_rev`. If the server has already applied that `op_id`, it returns the original `DocAck` or `DocConflict` instead of re-evaluating `base_rev`. So a lost acknowledgement never creates a spurious conflict copy. Genuine conflicts resolve as above. The first sign-in uploads the local documents. If the server already has documents, the user chooses "merge" or "keep server".

The demo book fills at the server's quotes, which come from Hyperliquid perp prices (§7). So the demo account must show the same `PERP` labelling, and its fills can happen off-hours, a behaviour change from today [unverified UX decision].

---

## 9. Moving the existing feeds server-side

| Feed | Today | Server |
|---|---|---|
| Congress (Senate eFD, House Clerk) | Each desktop ingests every 30 min. The web gets nothing. | One `CongressProvider` runs `tr_congress::refresh` on schedule into the server DB. Topics: `Congress(Latest)`, `Congress(Ticker)`, `Congress(Politician)`. Tracking alerts are per-user pushes. The web gets the full feature. **Legal note:** the reports carry statutory limits on commercial use (Ethics in Government Act) [unverified for our use; needs legal review]. |
| SnapTrade | Partner keys (`clientId`, `consumerKey`) and the user secret sit on each machine. The desktop uses them for reads *and* for order preview, place and cancel (`crates/broker/src/trading.rs`). | **Two modes.** (a) *Server mode* (web, mobile, and opt-in desktop): partner keys live only on the server. Each Terminal user maps to a SnapTrade `userId` and an encrypted `userSecret`, and the server proxies only the read-only endpoints (accounts, positions, orders, activities) plus the Connection Portal link. No trading in this mode. (b) *Local mode* (desktop, today's behaviour, unchanged): the local credentials stay in the Keychain or the 0600 file, and the desktop keeps calling SnapTrade directly for reads and for its existing order paths. This design does not migrate or remove local credentials, so existing desktop trading keeps working. Moving trading server-side is out of scope (§1) and would need its own design. |
| Yahoo, Treasury, FRED, Alpaca, RSS | Client-side; the web gets sample data | Server-side adapters with `DisplayPolicy` (§3.1). Yahoo and RSS are off for public display until reviewed [unverified]. |
| Binance and Coinbase crypto | Client-side websockets | Server-side, `public_display: false` (§3.1). Hyperliquid core perps and spot become the default crypto source. |

---

## 10. Observability

- **Tracing:** `tracing` with JSON logs and OpenTelemetry export. Logs carry no secrets, tokens or raw IPs (hash client IPs), and no user document contents.
- **Prometheus metrics:**
  - **Node (per host):** block height, blocks per second, the `exchangeStatus` lag between L1 time and local time, the `hl-node` version, restarts, and new `visor_child_stderr` lines (§6.5). Also disk use and free space, output bytes per hour, network in/out, and the gap in heights between node A and node B.
  - **Ingest:** file-tail lag (block written → applied), last applied `block_number`, block gaps, snapshot self-check mismatches per coin, book rebuilds, trade dedupe counts, bars closed and repaired, and the age of the latest oracle and mark per coin.
  - **Hot store:** topics, refcounts, lingering topics, evictions, and staleness per instrument (p50/p99 and the worst 10).
  - **Gateway:** connections, subscriptions per connection, outbound queue depth, conflation drops, resyncs (by reason), slow-consumer disconnects, the server-added latency histogram (ingest event to socket write), and the active node (A or B) with failover count.
  - **Sync:** doc ops per second, conflicts by kind, and outbox replays.
  - **Jobs:** backfill progress (one-time), pruner runs and bytes freed, gap repairs, and congress refresh duration and result.
- **Health:** `/healthz` (process up) and `/readyz` (a fresh node stream on at least one node, registry loaded, DB writable).
- **Alerts:**
  - Node: `exchangeStatus` lag above 2 s, no new block for 5 s, disk above 70% (warn) or 85% (page), crash loops, or a version change.
  - Ingest: a snapshot mismatch, a block gap, or a failover.
  - General: staleness p99 above 10 s during the underlying's open hours, and the error rate.
- **Dashboards:** Grafana, or a hosted equivalent.

---

## 11. Deployment and hosting cost

**v1 topology (colocated, §6.3):**
- **Node host A** (Vultr bare metal, Tokyo) runs `hl-visor`, `hl-node`, `tr-ingest` **and the primary `tr-gateway` plus its database**. Ingest-to-gateway traffic stays on loopback.
- **Node host B** (Cherry Servers, Tokyo, Equinix TY15) runs `hl-visor`, `hl-node`, `tr-ingest` **and a warm-standby `tr-gateway`**:
  - B's ingest streams to A's gateway over WireGuard/mTLS across providers (a short in-city hop [unverified; measure]).
  - **Market data fails over automatically.** B builds its own books and bars (§6.6). If A's health check fails, DNS points clients at B, and they resync the market topics.
- **User writes have exactly one writer, with synchronous replication and an epoch fence:**
  - **One writer.** Document ops, their `(user_id, op_id)` results (§8) and demo-book commands are accepted by the **writer** gateway only. The writer is A in v1, holding writer epoch `e`. B's gateway rejects writes while it's a standby; clients keep them in their outbox (§8).
  - **Synchronous ack.** The writer sends `DocAck`/`DocConflict` only after the change **and its `op_id` result** are committed locally **and** confirmed durable on the other host, tagged with epoch `e`. So an acknowledged edit always exists on both hosts, and a client never drops from its outbox an edit the survivor lacks.
  - **If the other host can't confirm, the writer stops acking.** Clients queue edits offline (§8) until replication returns. Market data is unaffected. We trade write availability for never losing acknowledged edits [target; revisit after measuring].
  - **Promotion is operator-initiated in v1** (a runbook step, not a health check):
    1. The operator promotes B to epoch `e+1`.
    2. From then on, B refuses replication with any epoch `≤ e`.
    3. A can no longer get confirmations, so it **cannot ack any further write** even if some clients can still reach it. When A reconnects, it sees the higher epoch and demotes itself to standby. That prevents split-brain divergence of documents and the demo book.
  - Clients replay their outbox against the new writer, and `op_id` dedupe returns the original result for edits that had been acked (§8).
- **Bars and other derived data** are rebuilt per host from node output, so they need no write fence (§3.4, §6.6).
- **The gateway runs on the node boxes, not a separate cloud VM.** Node A has 24 cores / 48 threads and 256 GB, enough headroom for both [unverified until measured]. The node gets most of the cores via cgroup/`CPUAffinity` limits, so client load can't starve block processing.
  - If measurement shows contention, the gateway moves to a small box on the **same provider's private LAN** (Vultr VPC, Cherry private VLAN) in Tokyo. It never goes to another region.
- **Market data starts in Tokyo,** so the hop to North American users is the same whether the gateway is in Tokyo or North America. The cost is a longer RTT for document sync from North America, ~150 ms [unverified; to measure], which is fine for that traffic. North American edge gateways can come later, if measurement shows it matters.

| Item | Monthly cost (list, §6.3) |
|---|---|
| Node A: Vultr `vbm-24c-256gb-amd`, Tokyo (node + ingest + gateway) | **$725** |
| Node B: Cherry Servers EPYC 9275F, Tokyo (node + ingest + standby gateway) | **$874.17** |
| Extra bandwidth beyond Vultr's 10 TB, at $0.01/GB | ≈ $0–50 [unverified; depends on §5.9 measurements] |
| Object storage for our own fills archive (optional, §6.4) | ≈ $10–30 [unverified] |
| One-time S3 backfill, requester-pays transfer (§5.6) | One-off; unknown until the bucket size is measured [unverified] |
| **Total (two nodes, gateway colocated)** | **≈ $1,610–1,680** |
| Total (single-node budget fallback, §6.6) | ≈ $735–805 (Vultr only); ≈ $375–600 on a 16-core Latitude or Cherry box once Tokyo stock returns |

Egress estimate (per always-on client): a 50-symbol watchlist at 4 Hz × ~30 bytes per postcard update ≈ 6 KB/s ≈ **15 GB/month** worst case [unverified, rough]. Most clients are idle or backgrounded most of the time. We measure real numbers in phase 6.

**Recommendation:**
- Two bare-metal nodes on two providers in Tokyo (Vultr + Cherry Servers), with `tr-ingest` and `tr-gateway` colocated on the node boxes. Primary on A, warm standby on B.
- Containerise ingest and the gateway (one Dockerfile each). The node itself runs on the host under systemd, as the README expects.
- CI deploys from `master` tags, alongside the existing `deploy-site.yml`.

---

## 12. Phased plan (node first; small PRs, each proving one thing)

| # | PR / step | Proves |
|---|---|---|
| 0 | **This doc** | Agreement on scope, protocol, node plan and risks |
| 1 | **Node A live in Tokyo** (ops, not app code): an Ubuntu 24.04 host per §6.2, GPG-verified `hl-visor`, the flags in §5.1, the pruner, and node metrics and alerts. A runbook goes in `docs/ops/node.md` (no addresses or secrets). | The node syncs and `exchangeStatus` stays fresh. We measure GB/day, bandwidth and catch-up after a planned restart. **Acceptance tests for the §5.2 verdict:** `xyz:` and other HIP-3 coins present in raw book diffs and `l4Snapshots`, spot present, one book compared once against the public `l2Book`, local `activeAssetData.markPx` working, and the oracle actions located in `replica_cmds`. |
| 2 | `crates/hl-node`: parsers for node files (fills, order statuses, raw book diffs, HIP-3 oracle updates, misc events, batched blocks), from **recorded node fixtures**, plus the checkpointed file tailer | Our parsers match the real shapes. Resume-from-checkpoint is exact. |
| 3 | `crates/book` (`tr-book`): L4 from snapshot plus diffs, L2 and BBO aggregation, and a self-check against `fileSnapshot` | Books match fresh snapshots for 24 h with zero unrepaired mismatches, across core, HIP-3 and spot. |
| 4 | `tr-ingest`: tape dedupe, the candle builder (all intervals, no cap), the candle store, and prices (mid, last, oracle, mark, funding, §5.4) | Bars are deterministic and identical when built twice from the same files. Prices reconcile against the abci state. |
| 5 | `crates/proto` (`tr-proto`): message types, postcard and JSON codecs, round-trip and size tests, a `wasm32` build in CI | The schema compiles everywhere. Frame sizes are known. |
| 6 | `server/` (`tr-gateway`) skeleton: the ingest → gateway stream, axum WS, `Hello`/`Welcome`, ping, and the `Quotes` topic. A CLI client and a 1k-client load test. | **One ingest fans out to N clients.** Server-added latency, internal-stream bandwidth and egress are measured. |
| 7 | Symbol registry (from the local info server) and mapping (§7), `Resolution` in quotes, and `RemoteMarkets` in `tr-data` behind `TR_SERVER_URL`. The web build uses it. | The web build shows live `xyz:` and core prices with `PERP` labelling. The `TODO(web proxy)` stubs are gone for quotes. |
| 8 | The `history` RPC and live `Bars` with `BarClosed`, plus `Book` (L2 and L4), `Trades` and `Ctx` topics, linger and eviction, and slow-client backpressure tests | Charts and books work through the server. Slow clients don't hurt others. |
| 9 | **One-time backfill** (§5.6): measure the S3 bucket, rebuild from `node_fills_by_block`, run the `candleSnapshot` sweep for what's left, and stitch with provenance | History before node start exists. Overlap comparisons pass. |
| 10 | **Node B and failover** (§6.6) | Killing node A mid-session causes one `Resync` and no lost closed bars. |
| 11 | Auth (sign-in) and accounts. Document sync for layouts, prefs and watchlists, with the conflict rules. | Two devices stay in sync, and conflicts never lose work. |
| 12 | Drawings (stable ids) and a server-authoritative demo book | Real-time cross-device drawings. One paper book. |
| 13 | `CongressProvider` server-side. The web gets the Congress panes. | Congress data reaches the web and the stub is deleted. |
| 14 | `SnapTradeProvider` server-side (read-only server mode). Desktop local mode is left untouched. | Web and mobile get portfolios with no partner secrets in those clients. Desktop trading still works. |
| 15 | Staging and prod deploys for the gateway, plus dashboards and the full alert set | It runs unattended and we can see when it doesn't. |
| 16 | (Later) a licensed equities and options `Provider` (Intrinio, Twelve Data or Massive) | The provider interface holds up against a second real source. |

---

## 13. Risks

1. **Legal: Hyperliquid's Terms of Use.** The Terms (last updated June 15, 2026) define "Restricted Persons" to include persons and entities that "reside in, are located in, are incorporated in, or have a registered office in the United States of America or Ontario, Canada", and forbid them from using the Interface. They also prohibit circumventing restrictions, for example with VPNs [doc: TOU §1.6, §3.1.5]. The Terms cover the website *Interface*. They contain **no explicit license or prohibition for redistributing or displaying API data** [doc: TOU; I found no data-licensing clause]. Goshen Labs is in Toronto, and many of Terminal's users will be in the US. **This needs legal review before launch**, and it is the main reason the server is data-only. The Foundation node also excludes "certain activities involving the U.S. or Ontario" [doc: FND]. Running our own node doesn't change this analysis. It's a question about displaying Hyperliquid data, not about how we fetch it.
2. **Perp ≠ stock.** HIP-3 equity perps follow the underlying through an oracle during external sessions. Off-hours (weekends, holidays, and anything outside 24/5 coverage) the oracle follows trade.xyz's own order flow through an EMA, and mark is bounded around the last external price [doc: XYZ-ORACLE, XYZ-BOUNDS]. Prices *will* diverge from the listed stock: at the open, around earnings, and on corporate actions (splits and dividends handling [unverified]). Funding also moves the perp relative to spot. Mitigations: mandatory labelling (§7.1), session badges (§7.4), and never computing "stock" analytics (for example the Congress performance estimates) from perp prices without labelling them.
3. **Builder delistings and settlements.** HIP-3 deployers can halt and settle markets at any time, and can recycle names [doc: HIP3]. Validators can delist validator-operated perps [doc: DELIST]. Snapshot counts: 21 of 141 `xyz` markets and 56 of 235 core markets flagged delisted [observed]. Mitigations: epochs (§7.5), notifications, and keeping history.
4. **Dependence on one venue and one software stack.** Everything in v1 comes from one chain, and from `hl-node` binaries we can't audit. Output formats can change without notice. Mitigations: the `Provider` trait, staleness UI, recorded fixtures with parser tests in CI, and alerts on version changes.
5. **The node is a single point of failure** if we run only one. Mitigation: two nodes on two providers (§6.6). With one node, outages show as stale data until catch-up (§6.5).
6. **HIP-3 (and spot) book coverage is undocumented** (§5.2). If node book outputs leave out HIP-3 dexes, our core use case, HIP-3 equity perps, has no book source except the public API. Mitigation: the phase 1 acceptance test, run before any app code is built on top of it.
7. **The reference book code is unmaintained** [doc: LAT, OBS], and queue-priority edge cases exist [doc: LAT]. Mitigations: our own `tr-book` with snapshot self-checks, per-coin rebuilds and recorded-day replay tests.
8. **Core mark can't be fully derived from node data** (it uses external CEX perp mids [doc: MARK]). We rely on the local `activeAssetData` [unverified], with a labelled estimate as the fallback (§5.4).
9. **Disk and I/O.** Writing ~100–200 GB/day [doc: NODE; unverified total] can fill a disk within days if the pruner fails. Mitigations: checkpoint-gated pruning, 70%/85% alerts, and NVMe sized for headroom.
10. **Peers and auto-upgrades.** Startup depends on finding a good peer [doc: NODE]. `hl-visor` auto-upgrades `hl-node`, which can change output schemas or restart both nodes at once. Mitigations: reliable peers (§6.1), version-change alerts, and parser tests against fresh fixtures after every upgrade.
11. **Unknown backfill size and cost.** The S3 archive range and size are undocumented, and transfer is requester-pays [doc: HIST]. Mitigation: measure first. The `candleSnapshot` sweep is the cheap fallback.
12. **Display rights for other feeds.** Coinbase, OKX and Kraken (per the parent's research), Yahoo, and Congress data commercial-use limits. Mitigation: `DisplayPolicy` defaults to off.
13. **Thin HIP-3 liquidity.** Some markets are small (`abcd` had 1 asset [observed]). Mid prices can be noisy or missing when a book is empty [unverified for specific markets]. We fall back to mark price, with the basis label.
14. **A single stateful gateway.** Restarting it drops the hot store. Clients reconnect and resync, but it is visible. Mitigation: fast startup from the registry and SQLite, plus a warm-standby gateway on node host B (§11). Market data fails over automatically. User writes need synchronous replication with an epoch fence, and promotion is operator-initiated, so acknowledged edits are never lost and the two hosts never diverge. The cost is that edits queue in client outboxes while replication or promotion is pending. Later, a two-instance setup with sticky sessions. Colocation means a node-host outage also takes out that host's gateway; that's why the standby sits on the other provider.
15. **Protocol evolution with postcard.** A breaking change needs a version bump. Mitigations: the rules in §4.4 and support for N/N−1.

---

## 14. Hyperliquid facts referenced (summary)

| Fact | Value | Source |
|---|---|---|
| Node spec | Non-validator: 16 vCPU, 128 GB RAM, 500 GB SSD. Ubuntu 24.04 only. Ports 4001–4002 public. Tokyo for lowest latency. | [doc: NODE] |
| Latency spec | ≥ 32 logical cores, 128 GB RAM, 500 MB/s disk, a reliable peer, `--disable-output-file-buffering` | [doc: LAT] |
| Output volume | "Around 100 GB of logs per day" (more with order statuses and book diffs) | [doc: NODE] |
| Book outputs | `--write-raw-book-diffs`, `--write-order-statuses`, `--write-fills`, `--batch-by-block` | [doc: NODE, L1] |
| L4 checkpoint | `fileSnapshot` → `l4Snapshots` on the local info server (latest state), or `compute-l4-snapshots` from `periodic_abci_states` (every 10,000 blocks) | [doc: NODE] |
| L4 rebuild | Snapshot + apply order statuses = realtime L4 book | [doc: L1] |
| Reference builder | `order_book_server`: l2Book (≤ 100 levels), trades, l4Book. Unmaintained, no spot (per README), no trigger orders. | [doc: OBS, LAT] |
| Local info server | Subset only (`meta`, `spotMeta`, `perpDexs`, `exchangeStatus`, `activeAssetData`, …). No time series, no WS, no `l2Book`. | [doc: NODE] |
| Oracle | Validators publish about every 3 s. Weighted CEX spot median, then a stake-weighted median across validators. | [doc: ORACLE] |
| Mark | Median of oracle+EMA, the book/last-trade median, and CEX perp mids | [doc: MARK] |
| Funding | Hourly. Premium sampled every 5 s. Interest 0.01%/8h. Cap 4%/h. | [doc: FUNDING] |
| HIP-3 oracle | `setOracle` about every 3 s with `oraclePxs`, `markPxs` and `externalPerpPxs`. Node flag `--write-hip3-oracle-updates`. | [doc: HIP3-ACT, NODE] |
| S3 archives | Requester-pays. `hl-mainnet-node-data` (fills, trades, misc events, blocks). `hyperliquid-archive` (l2Book snapshots, asset_ctxs). No candles. | [doc: HIST] |
| Public candle depth (backfill only) | Most recent 5000 candles per interval | [doc: INFO] |
| Public REST weight (backfill only) | 1200/min per IP. `candleSnapshot` costs 20 plus 1 per 60 items. | [doc: RL] |
| HIP-3 naming | `{dex}:{coin}`. Asset id `100000 + dex_index*10000 + index` | [doc: ASSET, PERP] |

---

## 15. Open questions for Micky

1. **Legal:** who reviews the Hyperliquid ToS (US/Ontario "Restricted Persons") for a Toronto company displaying Hyperliquid data to US and Canadian users? Is it OK to bring the node and server up before that review, given that it's data-only?
2. **Node budget:** two nodes (≈ $1,600/month, list) or one node to start (≈ $725)? Is there a monthly ceiling?
3. **Hosting providers:** Vultr bare metal + Cherry Servers in Tokyo as the two-provider pair (§6.3), or wait for cheaper 16-core Latitude or Cherry stock in Tokyo? Freestyle, OVHcloud and Equinix Metal are ruled out.
4. **AWS account for the backfill:** OK to open or use one for requester-pays S3 transfer, and what's the cap on the one-time backfill spend?
5. **Gateway placement:** colocated on the node boxes in Tokyo (recommended for v1, §11), or on a separate same-LAN box from the start?
6. **Mark price:** OK to show the node-read mark (`activeAssetData`) and fall back to a labelled estimate if that doesn't work?
7. **If HIP-3 books are missing from node output** (the phase 1 test): pause, or allow the public `l2Book` for HIP-3 only, as a stopgap?
8. **Default crypto source:** core perp mid (`BTC`) or HL spot (`UBTC/USDC`), and how prominently do we label "perp" for crypto?
9. **Proxy mapping:** should `AAPL US Equity` resolve to `xyz:AAPL` by default (with the `PERP` badge), or only when the user types `xyz:AAPL`? And should other dexes (`flx:`, `km:`, …) be mapped when xyz lacks a ticker?
10. **Off-hours:** show the perp price over weekends as the "last" price, or freeze the equity quote at the Friday close and show the perp separately?
11. **Auth:** a hosted identity provider (Clerk, WorkOS, Supabase) or self-hosted OAuth in Rust? Which sign-in methods first?
12. **Anonymous use:** can signed-out users get live data (today's behaviour), or is sign-in required for the web build?
13. **Demo account:** OK to make it server-authoritative and let it fill off-hours at perp prices?
14. **Encoding:** OK with postcard only (JSON for debugging), given that every client is Rust?
15. **Secondary feeds:** keep Binance, Coinbase and Yahoo server-side but staff-only, or drop them from the server entirely?
16. **Congress data:** get legal sign-off on the commercial-use limits before serving it publicly from our server?
17. **Licensed vendor:** is there a preferred vendor (Intrinio, Twelve Data, Massive) and a timeline, so the `DisplayPolicy` and entitlement model can be shaped around its contract?

---

## References

- **RL**: Rate limits and user limits. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits
- **INFO**: Info endpoint. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint
- **PERP**: Info endpoint, Perpetuals. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals
- **WS**: Websocket. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket
- **WS-SUB**: Websocket subscriptions. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions
- **HB**: Timeouts and heartbeats. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/timeouts-and-heartbeats
- **ASSET**: Asset IDs. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids
- **HIP3**: HIP-3, builder-deployed perpetuals. https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals
- **DELIST**: Delisting. https://hyperliquid.gitbook.io/hyperliquid-docs/trading/delisting
- **HIST**: Historical data. https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data
- **LAT**: Optimizing latency. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/optimizing-latency
- **L1**: L1 data schemas. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/nodes/l1-data-schemas
- **FND**: Foundation non-validating node. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/nodes/foundation-non-validating-node
- **NODE**: hyperliquid-dex/node README. https://github.com/hyperliquid-dex/node
- **OBS**: hyperliquid-dex/order_book_server README. https://github.com/hyperliquid-dex/order_book_server
- **TOU**: Hyperliquid Terms of Use (last updated June 15, 2026). https://app.hyperliquid.xyz/terms
- **XYZ-US**: trade[XYZ] US stocks. https://docs.trade.xyz/perpetuals/markets/stocks/us
- **XYZ-ORACLE**: trade[XYZ] oracle price. https://docs.trade.xyz/perpetuals/mechanics/oracle-price
- **XYZ-BOUNDS**: trade[XYZ] discovery bounds. https://docs.trade.xyz/perpetuals/mechanics/discovery-bounds
- **XYZ-TAU**: trade[XYZ] oracle time constant updates. https://docs.trade.xyz/perpetuals/changelog/oracle-time-constant-updates
- **ORACLE**: Oracle. https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/oracle
- **MARK**: Robust price indices. https://hyperliquid.gitbook.io/hyperliquid-docs/trading/robust-price-indices
- **FUNDING**: Funding. https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding
- **HIP3-ACT**: HIP-3 deployer actions. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions
- **INFRA-PEERS**: Foundation non-validating node for infra providers. https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/nodes/foundation-non-validating-node-for-infra-providers
- **LATITUDE**: Latitude.sh pricing. https://www.latitude.sh/pricing
- **LAT-M4**: Latitude.sh m4.metal.medium (official page; $456 US list) https://www.latitude.sh/pricing/m4-metal-medium. Tokyo price and stock come from a third-party tracker: https://servermarket.dev/providers/latitude/m4-metal-medium
- **LAT-F4**: Latitude.sh f4.metal.medium, Tokyo price and stock (third-party tracker). https://servermarket.dev/providers/latitude/f4-metal-medium
- **VULTR-API**: Vultr public plans API (plans, monthly price and locations; `nrt` is Tokyo, `itm` is Osaka) and OS list, read 2026-10-29. https://api.vultr.com/v2/plans-metal, https://api.vultr.com/v2/plans, https://api.vultr.com/v2/os
- **VULTR-BM**: Vultr bare metal product page. https://www.vultr.com/products/bare-metal/
- **VULTR-PRICE**: Vultr pricing (bandwidth overage). https://www.vultr.com/pricing/
- **CHERRY-TYO**: Cherry Servers Tokyo dedicated servers (prices, Equinix TY15, 100 TB egress, Ubuntu 24.04, private VLANs). https://www.cherryservers.com/bare-metal-dedicated-servers/tokyo. Launch post: https://www.cherryservers.com/blog/cherry-servers-tokyo-location-live
- **CHERRY-9950X**: Cherry Servers Ryzen 9950X price and stock by location (third-party tracker). https://servermarket.dev/providers/cherry/amd-ryzen-9950x. Official page: https://www.cherryservers.com/pricing/dedicated-servers/amd-ryzen-9950x
- **AWS-PRICE**: AWS EC2 on-demand Linux prices for Asia Pacific (Tokyo), from the price data behind the official pricing page, read 2026-10-29. https://aws.amazon.com/ec2/pricing/on-demand/
- **OVH-REGIONS**: OVHcloud dedicated-server regions (APAC: Singapore, Mumbai, Sydney). https://www.ovhcloud.com/en/bare-metal/regions-availability/
- **OVH-APAC**: OVHcloud APAC traffic quotas and throttling. https://support.us.ovhcloud.com/hc/en-us/articles/4404330636819-All-About-APAC-Region-Servers
- **EQX-EOL**: Equinix Metal sunset (June 30, 2026). https://docs.equinix.com/goodbye-metal/
- **FREESTYLE**: Freestyle VM pricing and limits, and inbound TLS. https://www.freestyle.sh/docs/vms/pricing-and-limits, https://www.freestyle.sh/docs/vms/network/tls-inbound
- **AWS prices** (third-party trackers, accessed 2026-10-29, not quotes): r6i.4xlarge Tokyo https://vmprices.com and https://www.usage.ai, r7a.4xlarge https://cloudcostdb.com, EBS gp3 Tokyo summaries. Official structure: https://aws.amazon.com/ec2/pricing/on-demand/ and https://aws.amazon.com/ebs/pricing/

### Observations log (read-only calls, 2026-10-29 ~17:35 UTC)

- `POST /info {"type":"perpDexs"}` returned 11 entries (null plus 10 named dexes).
- `{"type":"meta"}` returned 235 universe entries (56 `isDelisted`). `{"type":"meta","dex":"xyz"}` returned 141 entries (21 `isDelisted`), all names `xyz:`-prefixed.
- `{"type":"spotMeta"}` returned 347 pairs and 511 tokens.
- `{"type":"allMids"}` returned 1399 keys (401 `@…` spot, 762 `#…` outcomes, no `dex:` keys). `{"type":"allMids","dex":"xyz"}` returned `xyz:` keys only.
- `candleSnapshot` (startTime 0) returned:
  - BTC 1m: 5104 bars (~3.5 days)
  - BTC 1h: 5002 bars (from 2026-04-04)
  - BTC 1d: 2263 bars (from 2020-08-19)
  - `xyz:AAPL` 1d: 343 bars (from 2025-11-21)
  - Numbers came back as strings.
- WS `wss://api.hyperliquid.xyz/ws`, 12 s window, subscriptions to `allMids{dex:xyz}`, `l2Book BTC`, `trades BTC`, `candle xyz:AAPL 1m`, `bbo xyz:AAPL`, `activeAssetCtx xyz:AAPL` and `activeAssetCtx @107`:
  - Each got a `subscriptionResponse`.
  - Message counts: l2Book 3, trades 14, activeAssetCtx 13, activeSpotAssetCtx 13, bbo 9, allMids 2, candle 2.
  - `ping` → `{"channel":"pong"}`.
  - Candle OHLCV came as strings.
- WS `allDexsAssetCtxs` returned ~122 KB messages (2 in 8 s). WS `fastAssetCtxs` returned 10 messages in 8 s (1.9–28 KB), with a first snapshot of 2240 coins after base64 and raw-DEFLATE decoding.
- No `exchange` endpoint calls, no orders, no user-specific subscriptions.
- `POST /info {"type":"allPerpMetas"}` returned per-dex universe sizes as listed in §5.7.
- Hosting research (2026-10-29): read-only GETs of the Vultr public plans and OS APIs (no account), plus AWS's public Tokyo on-demand price data. Nothing was ordered or provisioned.
