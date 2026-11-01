# tr-gateway

The Terminal data server. For now it carries Hyperliquid's public API (see "Interim: public feed" in `docs/architecture/server.md`). One set of upstream websocket connections serves every client, fanned out over the protocol in `crates/proto`.

## Run it

```sh
cargo run --release -p tr-gateway
```

It listens on `localhost:8787`. Clients connect to `ws://localhost:8787/ws`. `/healthz` says the process is up, `/readyz` says the upstream is connected and every venue's mids have arrived, `/metrics` is Prometheus text and `/stats` is JSON.

The desktop app uses it for crypto markets when `TR_SERVER_URL` is set:

```sh
TR_SERVER_URL=ws://localhost:8787 cargo run --release
```

The browser build uses it too. `scripts/build-web.sh` builds in `wss://terminal-gateway.style.dev/ws`, or `TR_SERVER_URL` when it is set (empty for none). To try a local gateway with a built page, open the page with `?server=ws://localhost:8787/ws`.

The gateway serves plain HTTP and websockets. For `wss://` on 443, put a TLS-terminating edge in front of it and bind to an address the edge can reach, for example `TR_GATEWAY_BIND=0.0.0.0:8080`. Clients accept `wss://` URLs. The gateway pings each client every 20 s so idle connections survive proxies. The hosted one runs on Freestyle; see `deploy/freestyle/README.md`.

## Restarts

A restarted gateway listens before it knows which markets exist. It learns that from the instrument list and the first mids from every venue, which usually takes a few seconds. Clients that reconnect in that window keep their subscriptions. Each one gets `Stale`, then `Live` and its snapshot once the gateway is ready. Requests wait up to 10 s and then fail with `Upstream` rather than `UnknownMarket`, so clients can retry them. If one venue is slower than the rest, the gateway gets ready without it after 10 s. Subscriptions to that venue's markets keep waiting for it, and requests for them fail with `Upstream` until it reports. The client in `crates/data/src/remote.rs` subscribes again, with backoff from 1 s to 30 s, to anything a server ended with `Unavailable { reason: Upstream }`, so it also recovers from older gateways that gave up on such subscriptions.

## Try it

```sh
cli() { cargo run -q --release -p tr-gateway --example cli -- "$@"; }
cli markets xyz:
cli watch --quotes BTC,xyz:TSLA --book BTC --trades BTC --bars BTC:1m --secs 30
cli load --clients 5 --book BTC --trades BTC --secs 30
cli stats
```

`watch` prints about one line a second per topic, then latency. Quote latency counts from the moment the server received the price, book and trade latency from Hyperliquid's own timestamps. `/stats` reports the part the server adds, from an upstream frame arriving to the client frame it causes being written.

## Settings

Environment variables, also listed by `tr-gateway --help`:

| Variable | Default | |
|---|---|---|
| `TR_GATEWAY_BIND` | `localhost:8787` | Listen address, `host:port` |
| `TR_HL_DEXES` | `all` | HIP-3 dexes besides the core dex, for example `xyz,flx` |
| `TR_UPSTREAM_CONNECTIONS` | 3 | Upstream websocket connections, 2 to 8 |
| `TR_UPSTREAM_SUBSCRIPTIONS` | 900 | Upstream subscriptions, at most 950 |
| `TR_UPSTREAM_MESSAGES_PER_MIN` | 1000 | Messages sent upstream a minute, at most 1800 |
| `TR_REST_WEIGHT_PER_MIN` | 800 | `info` weight a minute, at most 1100 |
| `TR_LINGER_SECS` | 20 | How long an unwatched market keeps its upstream streams |
| `TR_MAX_CLIENTS` | 1000 | Client connections |

## Hyperliquid's limits

Per IP, Hyperliquid allows 10 websocket connections, 30 new connections a minute, 1000 subscriptions, 2000 sent messages a minute and 1200 REST weight a minute. The gateway keeps its own budget below each one (the caps in the table, and 20 new connections a minute). A subscription past the budget gets `Unavailable { reason: Capacity }`. The gateway never rotates IPs, uses proxies or otherwise works around a limit (Hyperliquid's terms, §3.1.8).

- The gateway subscribes every dex's mids for good, on a connection of their own.
- It subscribes a market's book, trades and candles only while some client watches it, and drops them `TR_LINGER_SECS` after the last one leaves.
- Hyperliquid closes the whole connection when asked for a coin it doesn't list, so the gateway only subscribes coins that have a live mid.
- History comes from `candleSnapshot`, which serves only the most recent 5000 bars per interval.
- It makes read-only public calls only: `info` requests and public websocket channels. No `exchange` endpoint, no user data, no orders.
