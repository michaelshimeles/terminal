# Terminal data server: design

Status: **draft for review**. This PR adds no server code.
Author: Grok Bot, for Micky. Research date: 2026-10-29. Times are UTC unless marked.

Terminal today fetches all of its own data: Yahoo, Binance and Coinbase websockets, Treasury.gov, Senate eFD and the House Clerk, and SnapTrade. Each desktop process opens its own connections. The browser build serves sample data wherever a source blocks cross-origin requests (`TODO(web proxy)` in `crates/data/src/web.rs` and `crates/congress/src/db_web.rs`). This doc proposes the replacement Micky asked for: **one Rust server that every client (desktop, web, later mobile) connects to, so market data and user state sync in real time**. Hyperliquid is the first market-data source, and the design leaves a clean provider interface for licensed equities and options later.

How facts are marked:

- **[doc: …]** is a fact from official documentation. Sources are listed in [References](#references).
- **[observed]** is a fact from read-only calls I made against `api.hyperliquid.xyz` on 2026-10-29 around 17:35 UTC. These are snapshots, not guarantees.
- **[unverified]** is an assumption or estimate that nobody has checked yet. It needs confirming before we build on it.
- **[target]** is a goal we set for ourselves. Nobody has measured it yet.

---

## 1. Goals and non-goals

### Goals

1. **One upstream, many clients.** The server holds the venue connections. Clients only ever talk to the server. Upstream load depends on the number of *distinct instruments in use*, never on the number of clients (§5.1).
2. **The same data everywhere.** Desktop, web (WASM) and mobile see the same quotes, bars, books and user documents, with live updates.
3. **Hyperliquid first:** core perps, spot, and HIP-3 builder dexes (trade.xyz `xyz:` stocks, indices, FX and commodities, and the other dexes).
4. **A provider interface clean enough** that a licensed equities and options vendor (for example Intrinio, Twelve Data or Massive) slots in later with no protocol change. Each provider also carries a display-rights policy (§3.1).
5. **User sync:** accounts, layouts, watchlists, drawings, preferences and the demo (paper) portfolio, with explicit conflict rules.
6. **The web build gets real data.** This removes the `TODO(web proxy)` stubs.
7. **The terminal stays snappy.** The protocol is binary and conflated, and the UI thread never waits on the network. This keeps the existing worker model in `app/src/worker.rs`.

### Non-goals (for this design)

- **No trading through the server.** The server is data only. It never signs, routes or places orders on Hyperliquid or anywhere else. That includes the SnapTrade order paths in `crates/broker/src/trading.rs`, which stay client-side and out of scope. The reasons are the legal flag in §13 (Hyperliquid's Terms of Use) and Micky's steer. The demo account is paper trading against our own quotes and never touches a venue.
- No licensed stock or options data yet. Only the interface for it is in scope (§3.1).
- No public API for third parties. The protocol is for our own clients.
- No running of a Hyperliquid **validator**. A non-validator node is discussed in §6.
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
             ┌───────────────────────── tr-server (one Rust binary) ─────────────────────────┐
 Hyperliquid │  Ingest adapters        Normalizer        Hot store + fan-out      Gateway    │   clients
 WS + /info ─┼─► HlProvider ─────────► (tr-core types, ─► topic table (watch /  ─► WS (bin) ─┼─► desktop
 (later node)│   CongressProvider      MarketKey,         broadcast), refcounts,   auth, subs, │─► web (WASM)
 eFD / Clerk ┼─► SnapTradeProvider     PriceBasis)        conflation, seq nos      backpressure│─► mobile
 SnapTrade ──┼─► (future) EquityVendor                    │                                   │
             │                                            ▼                                   │
             │                         Persistence: candles/history (SQLite → Postgres),      │
             │                         users & documents (Postgres), congress (SQLite→PG)     │
             └───────────────────────────────────────────────────────────────────────────────┘
```

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
| `HlProvider` | Hyperliquid public WS and `/info` (later, a local node) | §5 and §6 |
| `CongressProvider` | Senate eFD and House Clerk | Reuses `tr-congress` unchanged (§9) |
| `SnapTradeProvider` | SnapTrade, read-only | Per-user and private, so it is not a market-data topic (§9) |
| `CryptoProvider` (existing) | Binance and Coinbase | Off for public display |
| `TreasuryProvider` (existing) | Treasury.gov, with FRED as fallback | Yield curves |

### 3.2 Normalizer

This converts provider payloads into the shared types in `tr-core` and `tr-data`, plus a few new fields:

- **Numbers.** Hyperliquid sends prices and sizes as decimal **strings** [doc: INFO, WS-SUB]. The docs type the websocket `Candle` fields as `number`, but I observed strings (`"o":"349.69"`) [observed]. So the parser accepts either. The normalizer parses to `f64` for the existing types. It keeps the original string only where exact decimals matter: tick or lot rounding in the demo book.
- **Time.** Hyperliquid candles have `t` (open, in ms) and `T` (close, in ms) [doc: INFO]. `Bar.ts` = `t / 1000`, which matches the `Bar` contract (open time in seconds).
- **Intervals.** Hyperliquid offers `1m 3m 5m 15m 30m 1h 2h 4h 8h 12h 1d 3d 1w 1M` [doc: INFO, WS-SUB]. Every app `Timeframe` (M1, M5, M15, M30, H1, H4, D1, W1, MN1) maps 1:1.
- **Books.** `levels: [bids, asks]` with `{px, sz, n}` [doc: WS-SUB] becomes `OrderBook { bids, asks, source: "Hyperliquid", simulated: false }`.
- **Price basis.** This is new and required for the perp-as-proxy mapping (§7). Every quote carries `basis: Mid | Mark | Oracle | Last`, `venue`, `instrument_kind: Spot | Perp | Equity | …`, plus `session: Regular | Extended | Closed | Internal24x7` when the provider knows it.

### 3.3 In-memory hot store with fan-out

- The store is a **topic table** keyed by `Topic` (for example `Quote(MarketKey)`, `Bars(MarketKey, Timeframe)`, `Book(MarketKey, depth, group)`, `Trades(MarketKey)`, `Ctx(MarketKey)`, `Congress(Latest)`, `Doc(UserId, DocId)`).
- **State topics** (quote, book, ctx, the forming bar) use `tokio::sync::watch` style "latest value wins". A slow reader skips intermediate values, which conflates naturally.
- **Event topics** (trades, congress alerts, document ops) use a bounded `broadcast` ring with a per-topic sequence number. A reader that falls behind the ring gets `Resync` (§4.3).
- Each topic keeps a **refcount** of client subscribers. The upstream subscription is created when the count goes from 0 to 1, and dropped after a **linger** period (start at 60 s [target]) once the count returns to 0. This absorbs pane flicker and reconnect storms.
- Snapshots are served from memory, so a new client gets the current book or quote without touching the upstream.

### 3.4 Persistence (candles and history)

- **The problem it solves:** `candleSnapshot` returns only the **most recent 5000 candles** [doc: INFO]. At 1m that is about 3.5 days. I observed BTC 1m going back about 3.5 days and BTC 1h back to 2026-04-04 [observed]. So deep intraday history only exists if **we record it ourselves**. The S3 archive has no candles: "No other historical data sets are provided via S3 (e.g. candles or spot asset data). You can use the API to record additional historical data sets yourself." [doc: HIST]
- **The recorder:** a background task sweeps `candleSnapshot` 1m for every instrument on the recorded list, often enough never to fall behind the 5000-bar window. A daily sweep already keeps up, and running it every 1–6 h [target] gives margin for outages. The live bar comes from websocket `candle` subscriptions, but only for instruments someone is viewing.
- **Higher timeframes:** the recorder stores Hyperliquid's own 1h and 1d candles as well. Aggregating from 1m is the fallback for gaps, which avoids drift from differences in how candles get aggregated. Daily depth varies by instrument: BTC 1d returned 2263 bars back to 2020-08-19, and `xyz:AAPL` 1d returned 343 bars back to 2025-11-21 [observed].
- **Storage:** v1 uses **SQLite (WAL) on a volume**, reusing the `BarCache` schema ideas. User data moves to Postgres when we need more than one server instance. Candles can move to Postgres or Timescale at the same time. Size estimate: ~300 recorded instruments × 1440 bars/day ≈ 432k rows/day. At about 50–100 bytes per row with indexes, that is roughly **20–45 MB/day, or 8–16 GB/year** [unverified, rough].
- **Also persisted:** congress trades, user documents, demo books and audit logs (§8). Books and trades are *not* persisted in v1.

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
    DocOp { id: ReqId, doc: DocId, base_rev: u64, op: DocOperation },   // user-sync writes (§8)
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
- **The server sends `Resync` itself** when an event topic's ring overflowed for that client (§5.3), or after upstream recovery (§5.4), because the server's own state may have jumped.
- **For state topics a delta is just the new value** (a quote or a book), so a lost delta is harmless and those topics never need a resync on overflow. Bars deltas carry the forming bar; a new `ts` means the previous bar closed. This is the same contract as today's `LiveFeed`, so `chart_view` merges with `Bar::absorb` unchanged.
- **Reconnect:** `Hello.resume` holds the previous session's resume token. The server re-attaches the subscriptions and sends a fresh `Snapshot` for each one. Clients never replay old deltas.

### 4.4 Encoding

| Option | Pros | Cons |
|---|---|---|
| **postcard** (serde, compact varint) | Smallest frames. no_std-friendly, so it builds fine for WASM. Same serde derives as today. | Not self-describing: fields can't be added to an existing struct without a version bump. |
| bincode 2 | Fast and mature | Larger than postcard for small numbers. Same evolution caveat. |
| JSON (serde_json) | Debuggable, browser devtools-friendly | 3–5× larger, and parse cost on the WASM main thread [unverified ratio] |
| MessagePack (rmp-serde) | Self-describing and compact-ish | Middle ground. Another dependency. |

**Recommendation: postcard over binary WebSocket frames for every client, with JSON as a negotiated debug encoding** (`Hello.encoding`). The web client is Rust compiled to WASM, so there is *no JavaScript consumer*: the usual "JSON for the web" argument doesn't apply here. Evolution rules: append-only enum variants, a new message variant (never a changed struct) for new fields, and `PROTOCOL_VERSION` checked in `Hello`/`Welcome`. The server supports N and N−1.

Compression: the browser and tungstenite support for permessage-deflate needs checking [unverified]. postcard plus conflation may make it unnecessary. We measure in phase 3.

### 4.5 Latency targets

| Path | Target |
|---|---|
| Upstream message received → written to client sockets (server-added, p50 / p99) | **< 5 ms / < 25 ms** [target] |
| Subscribe → Snapshot for a hot topic (already in memory) | **< 50 ms** plus RTT [target] |
| Subscribe → Snapshot for a cold topic (needs upstream) | **< 1 s** [target]. Upstream RTT dominates. |
| Quote fan-out cadence to UI | Default 4 Hz per watchlist. The visible chart's bar and book stream at upstream speed. [target] |

End to end, Hyperliquid itself dominates. `l2Book` is a snapshot feed pushed "on each block that is at least 0.5 since last push" [doc: WS-SUB] (the unit is missing in the docs, presumably seconds). `bbo` is sent "only if the bbo changes on a block" [doc: WS-SUB].

### 4.6 Client integration

- **`tr-data::remote::RemoteMarkets`** implements `MarketData` over one shared connection. `history` maps to an RPC, `quotes` to a short-lived or shared `Quotes` subscription, and `live` to a `Bars` subscription bridged into the existing `mpsc::Receiver<Bar>`.
- **Selection:** `TR_SERVER_URL` (or a settings toggle) picks `RemoteMarkets`. The direct providers stay as the offline and dev fallback (`TR_PROVIDER=sample` keeps working).
- **Web build:** `for_browser()` switches to `RemoteMarkets`. This closes the `TODO(web proxy)` stubs in `crates/data/src/web.rs` and `crates/congress/src/db_web.rs`.

---

## 5. Hyperliquid ingest, fan-out and backpressure

### 5.1 The rate-limit math (upstream is independent of client count)

Hyperliquid's limits are **per IP address** [doc: RL]:

| Limit | Value [doc: RL] |
|---|---|
| REST weight | 1200 per minute, aggregated |
| `info` weight 2 | `l2Book`, `allMids`, `clearinghouseState`, `orderStatus`, `spotClearinghouseState`, `exchangeStatus` |
| `info` weight 60 | `userRole` |
| `info` weight 20 | every other documented `info` request (`meta`, `perpDexs`, `metaAndAssetCtxs`, `spotMeta`, `candleSnapshot` base, …) |
| `candleSnapshot` | an additional weight "per 60 items returned" |
| `recentTrades`, `fundingHistory`, … | an additional weight per 20 items returned |
| WebSocket connections | max **10** |
| New WebSocket connections | max **30 per minute** |
| WebSocket subscriptions | max **1000** |
| Unique users in user-specific subscriptions | max **10** |
| Messages sent to Hyperliquid | max **2000 per minute**, across all WS connections |
| In-flight WS post messages | max **100** |
| Time-range responses | return at most **500 elements**; paginate by `startTime` [doc: INFO] |
| Idle WS | server closes after **60 s** without a message; send `{"method":"ping"}` [doc: HB] |

**Why the client count doesn't matter.** Only the server talks to Hyperliquid, from its own IP. Clients connect to *our* gateway. One upstream subscription per distinct `(channel, coin)` feeds every client that wants it:

```
upstream_subs = fixed_firehose + |{ (channel, coin) currently demanded by ≥1 client, plus lingering }|
```

- 10,000 clients all watching `xyz:AAPL` 1m bars, its book and its trades use **3** upstream subscriptions.
- The **fixed firehose** covers quotes for *every* instrument with no per-coin subscriptions:
  - `allDexsAssetCtxs` (1 subscription): asset contexts (mark, oracle, mid, funding, OI, prevDayPx, volume) for every perp dex [doc: WS-SUB]. I observed messages covering all 11 dexes. Each message was about 122 KB, and 2 messages arrived in 8 s. For scale, `allPerpMetas` listed core 235, `xyz` 141, `para` 39, `hyna` 25, `mkts` 24, `km` 23, `cash` 17, `flx` 16, `io` 16, `vntl` 15 and `abcd` 1 assets, delisted ones included [observed].
  - `fastAssetCtxs` (1 subscription): base64 raw-DEFLATE JSON of `{markPx, midPx}` for changed coins. "The first message is a snapshot, subsequent messages contain only coins that have updated" [doc: WS-SUB]. I observed a 2240-key first snapshot across perps, spot and outcomes, then 10 messages in 8 s, each 1.9–28 KB [observed].
  - `allMids` per dex (up to 11 subscriptions) is an alternative, or a cross-check. Spot mids are only included with the first perp dex [doc: WS-SUB, INFO].
  - Spot contexts come per coin through `activeAssetCtx` (the channel name observed is `activeSpotAssetCtx`) [observed].
- **Demand-driven per-coin channels:** `l2Book`, `trades`, `bbo`, `candle`, `activeAssetCtx`. The server enforces a **budget of 900 of the 1000** subscriptions [target]. The headroom covers resubscribe churn. Past the budget, the least-recently-demanded lingering subscriptions are evicted first. A new demand that still doesn't fit gets `Status{Unavailable{reason: UpstreamBudget}}` and degrades: the quote comes from the firehose and the book is polled over REST at weight 2.
- **Connections:** the subscriptions are spread over 4–6 of the 10 allowed sockets [target], about 150–250 each [unverified sweet spot], so one disconnect only drops part of the feed. Reconnects use jittered backoff and stay below 30 new connections per minute.
- **Outgoing messages:** the 2000/min cap covers subscribe, unsubscribe and ping. A cold start of ~300 subscriptions plus pings fits. The subscription manager uses a token bucket at 1500/min [target] to stay under it.
- **REST budget (token bucket at 1000/1200 weight per minute [target]):**
  - Registry refresh: `perpDexs` plus `meta` for each of the ~11 dexes plus `spotMeta` ≈ 13 × 20 = **260 weight**. Running it every 5 minutes uses about 52 weight per minute.
  - Candle backfill: a full 5000-bar `candleSnapshot` costs 20 + ⌈5000/60⌉ = **~104 weight**, *if* "additional rate limit weight per 60 items" means +1 per 60 [unverified interpretation]. So about 9 full backfills per minute. A daily recorder sweep of 1440 bars costs ~44 weight per coin. For 300 coins that is ~13,200 weight per day, about 9 weight per minute on average.
  - Cold chart opens hit our SQLite first, and only call Hyperliquid for the uncached tail.

### 5.2 Addressing (HIP-3, spot and outcomes)

- `perpDexs` returns `[null, {name, fullName, deployer, oracleUpdater, feeRecipient, assetToStreamingOiCap, …}, …]`. `null` is the first (core) perp dex [doc: PERP]. I observed 11 entries: `null`, `xyz` (XYZ), `flx` (Felix Exchange), `vntl` (Ventuals), `hyna` (HyENA), `km` (Markets by Kinetiq), `abcd` (ABCDEx), `cash` (dreamcash), `para` (Paragon), `mkts` (Markets By Kinetiq) and `io` (EntropyIO). The objects also carried `subDeployers`, `assetToFundingMultiplier`, `assetToFundingInterestRate` and `assetToFundingClamp`, which are not in the doc example [observed].
- `meta`, `metaAndAssetCtxs`, `allMids`, `clearinghouseState` and others take `"dex": "<name>"`. The empty string means the first perp dex [doc: PERP, INFO]. `allPerpMetas` returns every dex at once [doc: PERP].
- HIP-3 coin names are always `{dex}:{coin}`, for example `xyz:TSLA` [doc: ASSET, INFO]. `candleSnapshot` needs the prefix: "For HIP-3, you need to prefix with the dex name, e.g. "xyz:XYZ100"" [doc: INFO]. The WS `allMids` with `dex:"xyz"` returned only `xyz:`-prefixed keys plus a `dex` field [observed].
- Spot coins are `PURR/USDC` or `@{index}` (for example, HYPE is `@107`). "Some assets may be remapped on user interfaces", for example BTC/USDC in the app is UBTC/USDC on HyperCore [doc: INFO].
- Outcome (HIP-4) markets show up as `#<encoding>` keys [doc: ASSET]. I observed 762 `#` keys and 401 `@` keys among the 1399 keys of the default `allMids` [observed]. **Outcomes are out of scope** and are filtered at ingest. They also carry separate legal restrictions [doc: TOU §1.7].
- Snapshot on 2026-10-29 [observed]: the core `meta` had 235 entries, 56 flagged `isDelisted`. The `xyz` `meta` had 141 entries, 21 flagged `isDelisted`. `spotMeta` had 347 pairs and 511 tokens.

### 5.3 Backpressure

- Each client has a **bounded outbound queue** (start at 256 frames or 1 MB [target]).
- **State topics** are conflated per client. On each flush tick (or immediately, for the active chart), the server writes the latest value of every dirty topic, so a slow client gets fewer updates but never stale ones.
- **Event topics:** if a client's cursor falls behind the ring, the server drops that subscription's backlog and sends `Resync` plus a `Snapshot`. For trades, the snapshot is the last N trades.
- If the queue stays full for more than 10 s [target], the server closes the connection with a "slow consumer" code, and the client reconnects with `resume`.
- **Ingest is never blocked by a client.** Upstream readers only write into the hot store. Fan-out runs in per-connection tasks.

### 5.4 Upstream failure handling

- Hyperliquid says: "Disconnection from API servers may happen periodically and without announcement. Missed data during the reconnect will be present in the snapshot ack on reconnect" [doc: WS]. Time-series subscriptions acknowledge with `isSnapshot: true` [doc: WS-SUB].
- **On reconnect** the server re-subscribes everything, marks the affected topics `Stale` until the first message arrives, then sends `Resync` to subscribers of event topics. Bars are reconciled against `candleSnapshot` for the gap.
- **Heartbeats:** the server sends `{"method":"ping"}` every 30 s on quiet sockets [doc: HB].
- **Staleness** is tracked per instrument (now minus the last update). The UI shows a stale badge, reusing today's freshness indicator style.

---

## 6. Self-hosting a Hyperliquid non-validator node

Micky doesn't want to depend on public-API rate limits. Here is what running our own node gets us, based on the official docs and `hyperliquid-dex/node` [doc: NODE].

### 6.1 Requirements and cost

| Item | Value | Source |
|---|---|---|
| Non-validator specs | **16 vCPUs, 128 GB RAM, 500 GB SSD** | [doc: NODE] |
| For lowest *read* latency | "at least 32 logical cores, 128 GB RAM, and 500 MB/s disk throughput" | [doc: LAT] |
| OS | "Currently only Ubuntu 24.04 is supported" | [doc: NODE] |
| Network | Ports **4001 and 4002** open to the public for gossip, otherwise peers deprioritize the node | [doc: NODE] |
| Location | "For lowest latency, run the node in Tokyo, Japan" | [doc: NODE] |
| Disk growth | "around 100 GB of logs per day" with default settings, so archive or delete old files | [doc: NODE, L1] |
| Peers | Mainnet needs at least one root IP in `~/override_gossip_config.json`. `gossipRootIps` lists recent peers, and the README lists community root peers (mostly Japan) | [doc: NODE] |
| Bandwidth | **Not documented** | [unverified] |
| Binary | `hl-visor` (signed, GPG-verified) manages `hl-node` and auto-upgrades | [doc: NODE] |

Two notes on these numbers. The two official pages disagree on CPU (16 vs 32): 16 is the documented minimum, and 32 is the advice for latency. And because of the 100 GB/day of logs, the usable disk needs aggressive pruning or more than 500 GB.

**Rough monthly cost (Tokyo):** an AWS `r6i.4xlarge` (16 vCPU, 128 GiB) in ap-northeast-1 costs **$1.216/h on demand ≈ $888/month, or ≈ $588/month with a 1-year commitment** (third-party price trackers checked 2026-10-29, not the AWS page) [unverified against AWS]. On top of that come a 500 GB–1 TB SSD volume and egress, neither priced here [unverified]. A 32-vCPU box roughly doubles the compute. Bare metal in Tokyo may be cheaper [unverified]. **Budget about $700–1,100/month for one node, with no redundancy** [unverified, rough]. Hetzner, used for the gateway in §11, has no Tokyo region (its price lists cover DE, FI, US and SG) [doc: HETZNER].

### 6.2 What the node exposes locally

- **Files, not sockets**, under `~/hl/data`, enabled by flags [doc: NODE, L1]:
  - `--write-trades` writes `node_trades/hourly/{date}/{hour}`.
  - `--write-fills` writes fills in the API format, with `deployerFee` for HIP-3 fills.
  - `--write-order-statuses` writes every order status ("substantial amount of data").
  - `--write-raw-book-diffs` writes every L1 order diff, from which a full book can be rebuilt.
  - `--write-hip3-oracle-updates` writes every HIP-3 deployer oracle update.
  - `--write-misc-events` writes funding, ledger and staking events.
  - Block batching or streaming: `--batch-by-block`, `--stream-with-block-info`, `--disable-output-file-buffering`.
  - State snapshots every 10,000 blocks, plus `compute-l4-snapshots` for full order-level books.
- **A local info server** (`--serve-info`) at `http://localhost:3001/info`, in the API request and response format, for a **subset** of requests that depend only on local state. It includes `meta`, `spotMeta`, `perpDexs`, `exchangeStatus`, `openOrders`, `clearinghouseState`, `perpsAtOpenInterestCap`, `marginTable` and others. "Historical time series queries and websockets are not currently supported". `l2Book` is not supported, and `webData2` "does not compute assetCtxs" [doc: NODE]. So **candles, mark, oracle and funding contexts, and `allMids` are not available from the local info server.** We would compute them ourselves or keep using the public API.
- **No built-in websocket.** `hyperliquid-dex/order_book_server` is an example that tails the node's files and serves `l2Book` (up to 100 levels), `trades` and a new `l4Book` over WS. It is "not actively maintained" [doc: LAT], doesn't support spot books, and is "not written by the Hyperliquid Labs core team" [doc: OBS]. Its last push was 2026-07-15 [observed]. Whether it handles HIP-3 coins is [unverified].

### 6.3 Latency compared with the public API

- The docs' read-latency advice is to run a non-validator against a reliable peer, disable output buffering, and build the book locally, "which has faster and more granular data than the API" [doc: LAT]. **No numbers are given for node vs public API** [unverified]. The one documented figure is that `split_client_blocks` mempool streaming "can lead to 70-150ms improvement in latency when reading inputs", but it carries *uncommitted* transactions with no execution results [doc: LAT].
- The Foundation non-validator (AWS apne1-az1) is a low-latency peer, but access requires staking 10,000 HYPE, Tier 1+ maker volume, and not being subject to the US/Ontario restrictions [doc: FND]. **We likely aren't eligible** (§13).
- For a human-facing terminal, the bigger latency cost is the client's distance to our gateway. A Tokyo node feeding a North America gateway adds a trans-Pacific hop [unverified RTT; to measure].

### 6.4 HIP-3 coverage

HIP-3 dexes run on the same HyperCore L1, and "the API to trade HIP-3 perps is unified with other HyperCore actions" [doc: HIP3]. The node's outputs include HIP-3 explicitly: the `deployerFee` field on HIP-3 fills, a dedicated `--write-hip3-oracle-updates` flag, and `perpDexs` and `meta` on the local info server [doc: NODE]. So **HIP-3 markets are included in node data** (`xyz:` trades, fills and book diffs). Whether every `--write-*` stream covers every dex identically is [unverified] until we run one.

### 6.5 Operational burden

- **A big machine** that we patch and monitor, on Ubuntu 24.04 only, with two public ports. Validators are told to aim for 98% uptime; that rule doesn't bind us, but it shows the standard [doc: FND].
- **About 100 GB/day of output** needs rotation, pruning and alerting on disk usage.
- **We'd write and maintain an ingest layer:** tailing hourly files, rebuilding books from raw diffs, computing candles from trades, and computing mark and oracle (or fetching them). The reference order-book server is unmaintained.
- **Upgrades are automatic** through `hl-visor`, which also means unplanned behaviour changes. Bootstrap means finding a peer and catching up.
- **It is a single point of failure** unless we run two nodes, which doubles the cost.

### 6.6 Recommendation

**Start with the public WebSocket fan-out, and add the node later.** Reasons:

1. The fan-out design (§5.1) already makes upstream load independent of client count. The 1000-subscription and 1200-weight budgets are only at risk if more than ~900 distinct `(channel, coin)` streams are viewed *at the same moment*, and the firehose subscriptions already cover quotes for everything.
2. The node doesn't replace the public API for the things a terminal needs most: candle history, asset contexts and `allMids` (§6.2).
3. At roughly $700–1,100/month plus real engineering and ops time, against roughly $50–70/month for the gateway (§11), the node only pays off at scale.

**Add the node when any of these holds** [target triggers]: sustained subscription-budget use above 80%, frequent upstream throttling or disconnects, a product need for deep or L4 books or full trade tape history, or a regulatory reason to reduce reliance on a third-party API operator. The `Provider` trait makes it a drop-in: `HlNodeProvider` serves trades, books and fills, while `HlProvider` (public API) keeps serving candles and contexts.

---

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

The registry refreshes `perpDexs` and `meta` for each dex every 5 minutes (§5.1). If an instrument turns `isDelisted`, or disappears, the server marks the topic `Delisted`, stops live streams, keeps its history, and notifies users who have it in watchlists or layouts. HIP-3 deployers can also halt and settle a market with `haltTrading`, which can later resume and "recycle" the asset [doc: HIP3]. That means the *same name may come back as a different contract*. History is therefore keyed by `(dex, coin, listing_epoch)`, and a relisting starts a new epoch [target].

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

Offline behaviour: clients keep today's local files as a cache and an outbox. On reconnect, they replay queued `DocOp`s with their `base_rev`, and conflicts resolve as above. The first sign-in uploads the local documents. If the server already has documents, the user chooses "merge" or "keep server".

The demo book fills at the server's quotes, which come from Hyperliquid perp prices (§7). So the demo account must show the same `PERP` labelling, and its fills can happen off-hours, a behaviour change from today [unverified UX decision].

---

## 9. Moving the existing feeds server-side

| Feed | Today | Server |
|---|---|---|
| Congress (Senate eFD, House Clerk) | Each desktop ingests every 30 min. The web gets nothing. | One `CongressProvider` runs `tr_congress::refresh` on schedule into the server DB. Topics: `Congress(Latest)`, `Congress(Ticker)`, `Congress(Politician)`. Tracking alerts are per-user pushes. The web gets the full feature. **Legal note:** the reports carry statutory limits on commercial use (Ethics in Government Act) [unverified for our use; needs legal review]. |
| SnapTrade (read-only) | Partner keys (`clientId`, `consumerKey`) and the user secret sit on each machine | Partner keys **live only on the server**. Each Terminal user maps to a SnapTrade `userId` and an encrypted `userSecret`. The server proxies the read-only endpoints (accounts, positions, orders, activities) and the Connection Portal link. Portfolio pushes on refresh. Order placement stays out (§1). |
| Yahoo, Treasury, FRED, Alpaca, RSS | Client-side; the web gets sample data | Server-side adapters with `DisplayPolicy` (§3.1). Yahoo and RSS are off for public display until reviewed [unverified]. |
| Binance and Coinbase crypto | Client-side websockets | Server-side, `public_display: false` (§3.1). Hyperliquid core perps and spot become the default crypto source. |

---

## 10. Observability

- **Tracing:** `tracing` with JSON logs and OpenTelemetry export. Logs carry no secrets, tokens or raw IPs (hash client IPs), and no user document contents.
- **Prometheus metrics:**
  - **Upstream:** messages per channel, reconnects, connections open (out of 10), subscriptions used (out of the 900 budget), REST weight used per minute (self-accounted), 429 or error responses, and the age of the last message per socket.
  - **Hot store:** topics, refcounts, lingering topics, evictions, and staleness per instrument (p50/p99 and the worst 10).
  - **Gateway:** connections, subscriptions per connection, outbound queue depth, conflation drops, resyncs (by reason), slow-consumer disconnects, and server-added latency histogram (upstream receive to socket write).
  - **Sync:** doc ops per second, conflicts by kind, and outbox replays.
  - **Jobs:** recorder sweep lag, candle gaps found and filled, and congress refresh duration and result.
- **Health:** `/healthz` (process up) and `/readyz` (upstream connected, registry loaded, DB writable).
- **Alerts:** upstream disconnected for more than 30 s, subscription budget above 80%, REST weight above 80%, staleness p99 above 10 s during the underlying's open hours, recorder lag above 12 h, error rate.
- **Dashboards:** Grafana, or a hosted equivalent.

---

## 11. Deployment and hosting cost

v1 is one stateful instance (hot store in memory, SQLite on a volume). It scales vertically first. Region: near our users (North America first), unless measurement shows the Hyperliquid API RTT matters more. The API servers' location is not documented in the pages I read [unverified]. The node docs point to Tokyo for *node* latency [doc: NODE].

| Option | What | Rough monthly cost |
|---|---|---|
| A. Hetzner Cloud | CCX13 (2 dedicated vCPU, 8 GB, 80 GB NVMe) in Ashburn: €43.49/month excl. VAT and IPv4 (post-15-June-2026 price list) [doc: HETZNER], plus a backup volume. Traffic allowance quoted as 1 TB or 20 TB depending on the listing [unverified]. | **≈ €45–60** |
| B. Fly.io | `performance-2x` 4 GB at $66.00/month, plus a volume at $0.15/GB/month, egress at $0.02/GB (NA/EU), and optional Managed Postgres from $38/month [doc: FLY] | **≈ $70–120** before egress |
| C. Later, with our own node | §6: plus ≈ $700–1,100 for a Tokyo node | **≈ $800–1,200** |

Egress estimate (per always-on client): a 50-symbol watchlist at 4 Hz × ~30 bytes per postcard update ≈ 6 KB/s ≈ **15 GB/month** worst case [unverified, rough]. Most clients are idle or backgrounded most of the time. At Fly's $0.02/GB that is about $0.30 per heavy client per month. On Hetzner it is mostly covered by the included traffic. We measure real numbers in phase 3.

**Recommendation:** Option A or B for v1. B if we want managed TLS, deploys and Postgres quickly; A if cost matters most. Containerise either way (one Dockerfile), with CI deploys from `master` tags, alongside the existing `deploy-site.yml`.

---

## 12. Phased plan (small PRs, each proving one thing)

| # | PR | Proves |
|---|---|---|
| 0 | **This doc** | Agreement on scope, protocol and risks |
| 1 | `crates/proto` (`tr-proto`): message types, postcard and JSON codecs, round-trip and size tests, `wasm32` build in CI | The schema compiles everywhere. Frame sizes are known. |
| 2 | `crates/hl`: Hyperliquid REST client (`meta`, `perpDexs`, `spotMeta`, `allMids`, `candleSnapshot`, `metaAndAssetCtxs`) with a weight-accounting token bucket, plus WS parsing from **recorded fixtures**. No server yet. | Our parsers match the real shapes, including string numbers and `activeSpotAssetCtx`. The weight math holds. |
| 3 | `server/` (`tr-server`) skeleton: axum WS gateway, `Hello`/`Welcome`, ping, and one topic (`Quotes`) fed by `allDexsAssetCtxs` and `fastAssetCtxs`. A CLI client and a load test with 1k simulated clients. | **One upstream fans out to N clients.** Server-added latency and egress are measured. |
| 4 | Symbol registry and mapping table (§7), `Resolution` in quotes, and `RemoteMarkets` in `tr-data` behind `TR_SERVER_URL`. The web build uses it. | The web build shows live `xyz:` and core prices with the `PERP` labelling. The `TODO(web proxy)` stubs are gone for quotes. |
| 5 | Bars: `history` RPC over `candleSnapshot` plus SQLite, the live `Bars` topic from WS `candle`, and the recorder sweep | Charts work through the server. History is retained past the 5000-bar window. |
| 6 | On-demand `Book`, `Trades`, `Ctx` and `bbo`, plus the subscription budget manager, linger and eviction, and backpressure tests with slow clients | The budget holds under churn. Slow clients don't hurt others. |
| 7 | Auth (sign-in) and accounts. Document sync for layouts, prefs and watchlists, with the conflict rules. | Two devices stay in sync, and conflicts never lose work. |
| 8 | Drawings (stable ids) and a server-authoritative demo book | Real-time cross-device drawings. One paper book. |
| 9 | `CongressProvider` server-side. The web gets the Congress panes. | Congress data reaches the web and the stub is deleted. |
| 10 | `SnapTradeProvider` server-side, with partner keys only on the server | No partner secrets in clients. |
| 11 | Deploy (staging and prod), metrics, dashboards and alerts | It runs unattended and we can see when it doesn't. |
| 12 | (Later) `HlNodeProvider` (§6) and a licensed equities and options `Provider` | The provider interface holds up against a second real source. |

---

## 13. Risks

1. **Legal: Hyperliquid's Terms of Use.** The Terms (last updated June 15, 2026) define "Restricted Persons" to include persons and entities that "reside in, are located in, are incorporated in, or have a registered office in the United States of America or Ontario, Canada", and forbid them from using the Interface. They also prohibit circumventing restrictions, for example with VPNs [doc: TOU §1.6, §3.1.5]. The Terms cover the website *Interface*. They contain **no explicit license or prohibition for redistributing or displaying API data** [doc: TOU; I found no data-licensing clause]. Goshen Labs is in Toronto, and many of Terminal's users will be in the US. **This needs legal review before launch**, and it is the main reason the server is data-only. The Foundation node also excludes "certain activities involving the U.S. or Ontario" [doc: FND].
2. **Perp ≠ stock.** HIP-3 equity perps follow the underlying through an oracle during external sessions. Off-hours (weekends, holidays, and anything outside 24/5 coverage) the oracle follows trade.xyz's own order flow through an EMA, and mark is bounded around the last external price [doc: XYZ-ORACLE, XYZ-BOUNDS]. Prices *will* diverge from the listed stock: at the open, around earnings, and on corporate actions (splits and dividends handling [unverified]). Funding also moves the perp relative to spot. Mitigations: mandatory labelling (§7.1), session badges (§7.4), and never computing "stock" analytics (for example the Congress performance estimates) from perp prices without labelling them.
3. **Builder delistings and settlements.** HIP-3 deployers can halt and settle markets at any time, and can recycle names [doc: HIP3]. Validators can delist validator-operated perps [doc: DELIST]. Snapshot counts: 21 of 141 `xyz` markets and 56 of 235 core markets flagged delisted [observed]. Mitigations: epochs (§7.5), notifications, and keeping history.
4. **Dependence on one venue.** Everything in v1 comes from one chain and one API operator. Outages, API changes (for example, the docs warn that "additional undocumented fields in WebData3 will be removed on a future update" [doc: WS-SUB]), or rate-limit changes hit every user at once. Mitigations: the `Provider` trait, staleness UI, and recorded history. Later: a node and a licensed vendor.
5. **Rate-limit ceilings.** These are per IP and could change. Scaling *horizontally* means more IPs, but splitting load across IPs only to evade limits conflicts with the spirit of the ToS (§3.1.8 forbids bots that "bypass rate limits") [doc: TOU]. The sanctioned path is our own node (§6).
6. **Display rights for other feeds.** Coinbase, OKX and Kraken (per the parent's research), Yahoo, and Congress data commercial-use limits. Mitigation: `DisplayPolicy` defaults to off.
7. **Thin HIP-3 liquidity.** Some markets are small (`abcd` had 1 asset [observed]). Mid prices can be noisy or missing when a book is empty [unverified for specific markets]. We fall back to mark price, with the basis label.
8. **A single stateful server.** Restarting it drops the hot store. Clients reconnect and resync, but it is visible. Mitigation: fast startup from the registry and SQLite, and a later move to a two-instance setup with sticky sessions.
9. **Protocol evolution with postcard.** A breaking change needs a version bump. Mitigations: the rules in §4.4 and support for N/N−1.

---

## 14. Hyperliquid facts referenced (summary)

| Fact | Value | Source |
|---|---|---|
| REST weight | 1200/min per IP. `l2Book`/`allMids` = 2. Most `info` = 20. `candleSnapshot` + per 60 items. | [doc: RL] |
| WS limits | 10 connections, 30 new/min, 1000 subscriptions, 2000 msgs/min sent, 10 unique users, 100 in-flight posts | [doc: RL] |
| WS idle timeout | 60 s. `{"method":"ping"}` gets `{"channel":"pong"}` | [doc: HB] |
| Candle depth | Most recent 5000 candles. Intervals 1m to 1M. | [doc: INFO] |
| Time-range pagination | 500 elements per response | [doc: INFO] |
| l2Book | Up to 20 levels per side. WS: 5 levels if `fast`, 20 if slow. Pushed per block, at least 0.5 apart. | [doc: INFO, WS-SUB] |
| HIP-3 naming | `{dex}:{coin}`. `dex` param, `""` = first dex. Asset id `100000 + dex_index*10000 + index` | [doc: ASSET, PERP] |
| Aggregate feeds | `allDexsAssetCtxs`, `fastAssetCtxs` (raw DEFLATE + base64) | [doc: WS-SUB] |
| No candles in S3 | Record them yourself | [doc: HIST] |
| Node | 16 vCPU, 128 GB, 500 GB SSD, Ubuntu 24.04, ports 4001–4002, Tokyo, ~100 GB/day logs | [doc: NODE] |
| Local info server | Subset only. No time series, no WS, no `l2Book` | [doc: NODE] |

---

## 15. Open questions for Micky

1. **Legal:** who reviews the Hyperliquid ToS (US/Ontario "Restricted Persons") for a Toronto company displaying Hyperliquid data to US and Canadian users? Is it OK to launch the server before that review, given that it is data-only?
2. **Default crypto source:** core perp mid (`BTC`) or HL spot (`UBTC/USDC`), and how prominently do we label "perp" for crypto?
3. **Proxy mapping:** should `AAPL US Equity` resolve to `xyz:AAPL` by default (with the `PERP` badge), or only when the user types `xyz:AAPL`? And should other dexes (`flx:`, `km:`, …) be mapped when xyz lacks a ticker?
4. **Off-hours:** show the perp price over weekends as the "last" price, or freeze the equity quote at the Friday close and show the perp separately?
5. **Auth:** a hosted identity provider (Clerk, WorkOS, Supabase) or self-hosted OAuth in Rust? Which sign-in methods first?
6. **Anonymous use:** can signed-out users get live data (today's behaviour), or is sign-in required for the web build?
7. **Demo account:** OK to make it server-authoritative and let it fill off-hours at perp prices?
8. **Hosting:** Hetzner (cheapest) or Fly.io (managed)? Which region? Should I measure RTT to `api.hyperliquid.xyz` from each first?
9. **Our own node:** agree to start with the public API and add the node on the triggers in §6.6? What monthly budget ceiling applies?
10. **Encoding:** OK with postcard only (JSON for debugging), given that every client is Rust?
11. **Secondary feeds:** keep Binance, Coinbase and Yahoo server-side but staff-only, or drop them from the server entirely?
12. **Congress data:** get legal sign-off on the commercial-use limits before serving it publicly from our server?
13. **Licensed vendor:** is there a preferred vendor (Intrinio, Twelve Data, Massive) and a timeline, so the `DisplayPolicy` and entitlement model can be shaped around its contract?

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
- **HETZNER**: Hetzner price adjustment, 15 June 2026. https://docs.hetzner.com/general/infrastructure-and-availability/price-adjustment/
- **FLY**: Fly.io resource pricing. https://fly.io/docs/about/pricing/

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
- `POST /info {"type":"allPerpMetas"}` returned per-dex universe sizes as listed in §5.1.
