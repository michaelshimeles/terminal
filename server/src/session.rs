//! One client connection (§3.5, §5.8): `Hello`/`Welcome`, then one forwarding task per
//! subscription feeding a bounded outbound queue that a writer task drains to the socket.
//!
//! - Every subscription has its own `seq`: a `Snapshot` sets it and each `Delta` adds one.
//! - State topics (quotes, book) conflate: a slow reader gets the latest value, never a backlog.
//! - Event topics (trades, bars) read a bounded ring. A reader that falls behind it gets
//!   `Resync` and a fresh `Snapshot`; a `Forming` bar may be dropped for a newer one with the
//!   same `ts`, a `Closed` bar never is.
//! - If the queue stays full for `slow_consumer`, the connection closes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tr_proto::{
    ApiError, BarDelta, Book, ClientMsg, Encoding, ErrorCode, Frame, Level, Limits, MarketKey, PROTOCOL_VERSION,
    PriceBasis, Quote, ResyncReason, Rpc, RpcResult, ServerMsg, SessionId, SubId, TopicData, TopicDelta, TopicReq,
    TopicStatus, UnavailableReason,
};

use crate::metrics::{Metrics, TopicKind as Kind};
use crate::store::{
    BarsTopic, BookTopic, Event, EventKind, Hub, Mid, Topic, TopicKey, TradesTopic, VenueState, merge_bars,
};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub max_subscriptions: usize,
    pub max_quote_keys: usize,
    pub max_book_depth: u8,
    pub max_history: u32,
    pub max_quote_hz: u8,
    /// One-shot requests a connection may have running at once.
    pub max_requests_in_flight: usize,
    /// A request still running after this gets an error reply.
    pub request_timeout: Duration,
    /// Client messages per 10 s. Subscribes past this would churn the shared upstream.
    pub max_messages_per_10s: u32,
    /// Outbound frames a client may have queued.
    pub queue: usize,
    pub hello_timeout: Duration,
    pub slow_consumer: Duration,
    /// Websocket pings keep idle connections open through proxies and TLS edges.
    pub ping_every: Duration,
    /// Close a client we've heard nothing from (not even a pong) for this long.
    pub idle_timeout: Duration,
    /// How long a request waits for the upstream after a restart before it fails. Subscriptions
    /// wait as long as it takes.
    pub ready_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_subscriptions: 64,
            max_quote_keys: 512,
            max_book_depth: 20,
            max_history: 5000,
            max_quote_hz: 10,
            max_requests_in_flight: 8,
            request_timeout: Duration::from_secs(30),
            max_messages_per_10s: 200,
            queue: 256,
            hello_timeout: Duration::from_secs(10),
            slow_consumer: Duration::from_secs(10),
            ping_every: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(90),
            ready_timeout: Duration::from_secs(10),
        }
    }
}

impl SessionConfig {
    pub fn limits(&self) -> Limits {
        Limits {
            max_subscriptions: self.max_subscriptions as u32,
            max_quote_keys: self.max_quote_keys as u32,
            max_book_depth: self.max_book_depth,
            max_history: self.max_history,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

struct Out {
    msg: Message,
    /// When the upstream data in this frame arrived, if it carries any.
    recv: Option<(Instant, Kind)>,
}

fn to_message(f: Frame) -> Message {
    match f {
        Frame::Binary(b) => Message::Binary(b.into()),
        Frame::Text(t) => Message::Text(t.into()),
    }
}

/// What every forwarding task of one session shares.
#[derive(Clone)]
struct Link {
    enc: Encoding,
    out: mpsc::Sender<Out>,
    kill: Arc<watch::Sender<bool>>,
    slow: Duration,
    metrics: Arc<Metrics>,
}

impl Link {
    /// Queue a message. False once the session is gone or too slow to keep.
    async fn send(&self, msg: &ServerMsg, recv: Option<(Instant, Kind)>) -> bool {
        let frame = match tr_proto::encode(msg, self.enc) {
            Ok(f) => to_message(f),
            Err(e) => {
                tracing::error!("encode: {e}");
                return true;
            }
        };
        match tokio::time::timeout(self.slow, self.out.send(Out { msg: frame, recv })).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                self.kill.send_replace(true);
                false
            }
        }
    }
}

/// One subscription's forwarding state.
struct Fwd {
    id: SubId,
    kind: Kind,
    link: Link,
    seq: u64,
    resync: Arc<Notify>,
}

impl Fwd {
    /// Snapshots carry state of any age, so they don't count towards the latency histograms.
    async fn snapshot(&mut self, data: TopicData) -> bool {
        self.seq += 1;
        let m = ServerMsg::Snapshot { id: self.id, seq: self.seq, data };
        self.link.send(&m, None).await
    }

    async fn delta(&mut self, data: TopicDelta, recv: Instant) -> bool {
        self.seq += 1;
        let m = ServerMsg::Delta { id: self.id, seq: self.seq, data };
        self.link.send(&m, Some((recv, self.kind))).await
    }

    async fn status(&self, status: TopicStatus) -> bool {
        self.link.send(&ServerMsg::Status { id: self.id, status }, None).await
    }

    async fn resync(&self, reason: ResyncReason) -> bool {
        Metrics::add(&self.link.metrics.resyncs, 1);
        self.link.send(&ServerMsg::Resync { id: self.id, reason }, None).await
    }

    async fn unavailable(&self, reason: UnavailableReason, detail: String) -> bool {
        self.status(TopicStatus::Unavailable { reason, detail }).await
    }
}

struct SubCtl {
    task: JoinHandle<()>,
    resync: Arc<Notify>,
}

/// Serve one client until it leaves, misbehaves or can't keep up.
pub async fn run(
    socket: WebSocket,
    hub: Arc<Hub>,
    cfg: Arc<SessionConfig>,
    session: u64,
    closing: watch::Receiver<bool>,
) {
    let metrics = hub.metrics.clone();
    metrics.clients.fetch_add(1, Ordering::Relaxed);
    Metrics::add(&metrics.clients_total, 1);
    serve(socket, hub, cfg, session, closing).await;
    metrics.clients.fetch_sub(1, Ordering::Relaxed);
}

fn decode(msg: &Message) -> Option<Result<ClientMsg, tr_proto::CodecError>> {
    match msg {
        Message::Binary(b) => Some(tr_proto::decode_binary(b)),
        Message::Text(t) => Some(tr_proto::decode_text(t.as_str())),
        _ => None,
    }
}

async fn reject(mut socket: WebSocket, enc: Encoding, code: ErrorCode, message: String) {
    if let Ok(f) = tr_proto::encode(&ServerMsg::Error { code, message: message.clone() }, enc) {
        let _ = socket.send(to_message(f)).await;
    }
    let reason = message.chars().take(100).collect::<String>();
    let _ = socket.send(Message::Close(Some(CloseFrame { code: 1008, reason: reason.into() }))).await;
}

async fn serve(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    cfg: Arc<SessionConfig>,
    session: u64,
    mut closing: watch::Receiver<bool>,
) {
    if *closing.borrow_and_update() {
        return;
    }
    // Hello first, in either encoding.
    let first = tokio::time::timeout(cfg.hello_timeout, async {
        while let Some(Ok(m)) = socket.recv().await {
            if let Some(r) = decode(&m) {
                return Some((r, matches!(m, Message::Text(_))));
            }
        }
        None
    })
    .await;
    let (enc, client) = match first {
        Ok(Some((Ok(ClientMsg::Hello { version, encoding, client, .. }), _))) if version == PROTOCOL_VERSION => {
            (encoding, client)
        }
        Ok(Some((Ok(ClientMsg::Hello { version, encoding, .. }), _))) => {
            let msg = format!("protocol {version} is not supported; this server speaks {PROTOCOL_VERSION}");
            return reject(socket, encoding, ErrorCode::UnsupportedVersion, msg).await;
        }
        Ok(Some((_, text))) => {
            let enc = if text { Encoding::Json } else { Encoding::Postcard };
            return reject(socket, enc, ErrorCode::HelloRequired, "send Hello first".into()).await;
        }
        _ => return,
    };
    tracing::debug!("session {session}: {} {} ({enc:?})", client.name, client.version);
    let welcome = ServerMsg::Welcome {
        version: PROTOCOL_VERSION,
        session: SessionId(session),
        user: None,
        server_time_ms: now_ms(),
        limits: cfg.limits(),
    };
    let Ok(f) = tr_proto::encode(&welcome, enc) else { return };
    if socket.send(to_message(f)).await.is_err() {
        return;
    }

    let (ws_tx, mut ws_rx) = socket.split();
    let (out_tx, out_rx) = mpsc::channel::<Out>(cfg.queue.max(1));
    let (kill, mut killed) = watch::channel(false);
    let link = Link { enc, out: out_tx, kill: Arc::new(kill), slow: cfg.slow_consumer, metrics: hub.metrics.clone() };
    let mut writer = tokio::spawn(write(ws_tx, out_rx, hub.metrics.clone(), cfg.ping_every));
    let mut subs: HashMap<SubId, SubCtl> = HashMap::new();
    let mut idle = tokio::time::interval(cfg.idle_timeout / 3);
    let mut last_heard = Instant::now();
    let mut writer_done = false;
    let mut rpcs = tokio::task::JoinSet::new();
    let rate = crate::limit::WindowLimiter::new(cfg.max_messages_per_10s, Duration::from_secs(10));

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                last_heard = Instant::now();
                let Some(decoded) = decode(&msg) else {
                    if matches!(msg, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                if rate.try_acquire(1).is_err() {
                    let m = format!("more than {} messages in 10 s; this one was ignored", cfg.max_messages_per_10s);
                    link.send(&ServerMsg::Error { code: ErrorCode::RateLimited, message: m }, None).await;
                    continue;
                }
                match decoded {
                    Ok(m) => handle(m, &hub, &cfg, &link, &mut subs, &mut rpcs).await,
                    Err(e) => {
                        let m = ServerMsg::Error { code: ErrorCode::BadMessage, message: e.to_string() };
                        link.send(&m, None).await;
                    }
                }
            }
            _ = killed.changed() => {
                Metrics::add(&hub.metrics.slow_consumer_closes, 1);
                let m = ServerMsg::Error { code: ErrorCode::SlowConsumer, message: "too slow to keep up; reconnect".into() };
                // The queue is full; the close goes out on a fresh channel slot or not at all.
                let _ = link.out.try_send(Out { msg: to_message(tr_proto::encode(&m, enc).expect("encodes")), recv: None });
                break;
            }
            _ = idle.tick() => if last_heard.elapsed() > cfg.idle_timeout {
                tracing::debug!("session {session}: idle; closing");
                break;
            },
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            // Only ever set to true, or dropped with the gateway.
            _ = closing.changed() => break,
        }
    }
    for (_, s) in subs.drain() {
        s.task.abort();
    }
    rpcs.abort_all();
    drop(link);
    // Let the writer flush what's queued (a final error, the close frame), briefly.
    if !writer_done && tokio::time::timeout(Duration::from_secs(1), &mut writer).await.is_err() {
        writer.abort();
    }
}

async fn write(
    mut ws: futures::stream::SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Out>,
    metrics: Arc<Metrics>,
    ping_every: Duration,
) {
    let mut ping = tokio::time::interval(ping_every);
    ping.tick().await;
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(out) = out else { break };
                let len = match &out.msg {
                    Message::Binary(b) => b.len(),
                    Message::Text(t) => t.len(),
                    _ => 0,
                };
                if ws.send(out.msg).await.is_err() {
                    break;
                }
                Metrics::add(&metrics.frames_sent, 1);
                Metrics::add(&metrics.bytes_sent, len as u64);
                if let Some((recv, kind)) = out.recv {
                    metrics.latency_for(kind).record(recv.elapsed());
                }
            }
            _ = ping.tick() => if ws.send(Message::Ping(Default::default())).await.is_err() {
                break;
            },
        }
    }
    let _ = ws.close().await;
}

async fn handle(
    msg: ClientMsg,
    hub: &Arc<Hub>,
    cfg: &Arc<SessionConfig>,
    link: &Link,
    subs: &mut HashMap<SubId, SubCtl>,
    rpcs: &mut tokio::task::JoinSet<()>,
) {
    let error = |code, message: String| ServerMsg::Error { code, message };
    match msg {
        ClientMsg::Hello { .. } => {
            link.send(&error(ErrorCode::BadMessage, "Hello was already sent".into()), None).await;
        }
        ClientMsg::Subscribe { id, topic } => {
            if subs.contains_key(&id) {
                link.send(&error(ErrorCode::DuplicateId, format!("subscription {id} exists")), None).await;
                return;
            }
            if subs.len() >= cfg.max_subscriptions {
                let m = format!("at most {} subscriptions per connection", cfg.max_subscriptions);
                link.send(&error(ErrorCode::TooManySubscriptions, m), None).await;
                return;
            }
            let resync = Arc::new(Notify::new());
            let kind = match topic {
                TopicReq::Quotes { .. } => Kind::Quotes,
                TopicReq::Book { .. } => Kind::Book,
                TopicReq::Trades { .. } => Kind::Trades,
                TopicReq::Bars { .. } => Kind::Bars,
            };
            let fwd = Fwd { id, kind, link: link.clone(), seq: 0, resync: resync.clone() };
            let (hub, cfg) = (hub.clone(), cfg.clone());
            let metrics = hub.metrics.clone();
            let task = tokio::spawn(async move {
                metrics.client_subscriptions.fetch_add(1, Ordering::Relaxed);
                let _count = Decrement(metrics);
                forward(fwd, hub, cfg, topic).await;
            });
            subs.insert(id, SubCtl { task, resync });
        }
        ClientMsg::Unsubscribe { id } => match subs.remove(&id) {
            Some(s) => s.task.abort(),
            None => {
                link.send(&error(ErrorCode::UnknownId, format!("no subscription {id}")), None).await;
            }
        },
        ClientMsg::Resync { id } => match subs.get(&id) {
            Some(s) => s.resync.notify_one(),
            None => {
                link.send(&error(ErrorCode::UnknownId, format!("no subscription {id}")), None).await;
            }
        },
        ClientMsg::Request { id, req } => {
            while rpcs.try_join_next().is_some() {}
            if rpcs.len() >= cfg.max_requests_in_flight {
                let message = format!("at most {} requests in flight", cfg.max_requests_in_flight);
                let result = Err(ApiError { code: ErrorCode::RateLimited, message });
                link.send(&ServerMsg::Reply { id, result }, None).await;
                return;
            }
            let (hub, cfg, link) = (hub.clone(), cfg.clone(), link.clone());
            // The set aborts whatever is still running when the session ends.
            rpcs.spawn(async move {
                let result = match tokio::time::timeout(cfg.request_timeout, rpc(&hub, &cfg, req)).await {
                    Ok(r) => r,
                    Err(_) => Err(ApiError { code: ErrorCode::Upstream, message: "timed out".into() }),
                };
                link.send(&ServerMsg::Reply { id, result }, None).await;
            });
        }
        ClientMsg::Ping { t } => {
            link.send(&ServerMsg::Pong { t, server_time_ms: now_ms() }, None).await;
        }
    }
}

/// Counts a subscription for as long as its task lives, however it ends.
struct Decrement(Arc<Metrics>);

impl Drop for Decrement {
    fn drop(&mut self) {
        self.0.client_subscriptions.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn rpc(hub: &Arc<Hub>, cfg: &SessionConfig, req: Rpc) -> Result<RpcResult, ApiError> {
    let err = |code, message: String| ApiError { code, message };
    // Not `UnknownMarket`, since clients stop asking about a market the server called unknown.
    if !hub.ready(cfg.ready_timeout).await {
        return Err(err(ErrorCode::Upstream, "the server is starting; no upstream data yet".into()));
    }
    match req {
        Rpc::History { key, tf, end, limit } => {
            let inst =
                hub.resolve(&key).ok_or_else(|| err(ErrorCode::UnknownMarket, format!("unknown market {key}")))?;
            let limit = (limit.clamp(1, cfg.max_history)) as usize;
            // With a live stream for this market, cached history must be newer than the stream's
            // last (re)start, or bars that closed while it was down could be missing or partial.
            let live = hub.bars_topic(&inst, tf).filter(|_| end.is_none());
            let fresh_after = live.as_ref().map(|t| t.overlay().live_since.unwrap_or_else(Instant::now));
            let bars = hub
                .history(&inst, tf, end, limit, fresh_after)
                .await
                .map_err(|e| err(ErrorCode::Upstream, format!("{e:#}")))?;
            // Recent history also gets whatever the live stream has seen since.
            let bars = match live {
                Some(t) => merge_bars(bars.to_vec(), &t.overlay().bars, limit),
                None => bars.to_vec(),
            };
            Ok(RpcResult::History(bars))
        }
        Rpc::Markets => Ok(RpcResult::Markets(hub.markets())),
    }
}

async fn forward(f: Fwd, hub: Arc<Hub>, cfg: Arc<SessionConfig>, topic: TopicReq) {
    // Right after a restart the gateway doesn't know yet which markets exist. A client that
    // reconnects then keeps its subscriptions, stale until the upstream is in and live after.
    if !hub.is_ready() {
        if !f.status(TopicStatus::Stale { since_ms: now_ms() }).await {
            return;
        }
        hub.wait_ready().await;
        if !f.status(TopicStatus::Live).await {
            return;
        }
    }
    let unknown = |k: &MarketKey| (UnavailableReason::UnknownMarket, format!("unknown market {k}"));
    let (tkey, extra) = match &topic {
        TopicReq::Quotes { keys, max_hz } => {
            if keys.len() > cfg.max_quote_keys {
                let m = format!("at most {} keys per quotes subscription", cfg.max_quote_keys);
                f.unavailable(UnavailableReason::Unsupported, m).await;
                return;
            }
            let hz = (*max_hz).clamp(1, cfg.max_quote_hz);
            return quotes(f, hub, keys.clone(), hz).await;
        }
        TopicReq::Book { key, .. } => (hub.resolve(key).map(TopicKey::Book).ok_or_else(|| unknown(key)), 0),
        TopicReq::Trades { key } => (hub.resolve(key).map(TopicKey::Trades).ok_or_else(|| unknown(key)), 0),
        TopicReq::Bars { key, tf, history } => (
            hub.resolve(key).map(|i| TopicKey::Bars(i, *tf)).ok_or_else(|| unknown(key)),
            (*history).min(cfg.max_history),
        ),
    };
    let held = match tkey {
        Ok(k) => hub.acquire(k).await,
        Err(e) => Err(e),
    };
    let held = match held {
        Ok(h) => h,
        Err((reason, detail)) => {
            f.unavailable(reason, detail).await;
            return;
        }
    };
    match (&held.topic, topic) {
        (Topic::Book(t), TopicReq::Book { depth, group, .. }) => {
            book(f, t.clone(), depth.clamp(1, cfg.max_book_depth) as usize, group).await
        }
        (Topic::Trades(t), TopicReq::Trades { .. }) => trades(f, t.clone()).await,
        (Topic::Bars(t), TopicReq::Bars { .. }) => bars(f, &hub, t.clone(), extra as usize).await,
        _ => unreachable!("the topic key matches the request"),
    }
    drop(held);
}

fn quote_for(hub: &Hub, key: &MarketKey, coin: &str, m: &Mid) -> Quote {
    Quote {
        key: key.clone(),
        source: MarketKey::new(coin),
        px: m.px,
        basis: PriceBasis::Mid,
        prev_day: hub.prev_day(coin),
        ts_ms: m.ts_ms,
    }
}

/// The worst state among a subscription's venues: unavailable, else stale (since the earliest
/// outage), else live.
fn combined(states: &[VenueState]) -> TopicStatus {
    let mut out = TopicStatus::Live;
    for st in states {
        match (&st.status, &out) {
            (TopicStatus::Unavailable { .. }, _) => return st.status.clone(),
            (TopicStatus::Stale { since_ms }, TopicStatus::Stale { since_ms: s }) if since_ms >= s => {}
            (TopicStatus::Stale { .. }, _) => out = st.status.clone(),
            _ => {}
        }
    }
    out
}

async fn quotes(mut f: Fwd, hub: Arc<Hub>, keys: Vec<MarketKey>, hz: u8) {
    let resolved: Vec<(MarketKey, String)> =
        keys.into_iter().filter_map(|k| hub.resolve(&k).map(|i| (k, i.0))).collect();
    if resolved.is_empty() {
        f.unavailable(UnavailableReason::UnknownMarket, "none of these markets are known".into()).await;
        return;
    }
    // The feeds these quotes come from, to report their outages.
    let mut venues: Vec<String> =
        resolved.iter().filter_map(|(_, c)| hub.mids.get(c).map(|m| m.venue.to_string())).collect();
    venues.sort();
    venues.dedup();
    let venue_states = || venues.iter().map(|v| hub.mids.venue(v)).collect::<Vec<_>>();
    let mut epochs: Vec<u64> = venue_states().iter().map(|v| v.epoch).collect();
    let mut status = TopicStatus::Live;
    let mut version = hub.mids.subscribe();
    // What each requested key last carried: two keys can share a coin (`BTC`, `BTCUSD Curncy`).
    let mut stamps: Vec<u64> = vec![0; resolved.len()];
    let mut send_snapshot = true;
    let period = Duration::from_secs(1) / u32::from(hz);
    loop {
        version.borrow_and_update();
        let states = venue_states();
        let now = combined(&states);
        if now != status {
            status = now;
            if !f.status(status.clone()).await {
                return;
            }
        }
        // A feed that reconnected may have skipped prices: once it's live again, start over.
        let now_epochs: Vec<u64> = states.iter().map(|v| v.epoch).collect();
        if status == TopicStatus::Live && now_epochs != epochs {
            epochs = now_epochs;
            if !send_snapshot && !f.resync(ResyncReason::Upstream).await {
                return;
            }
            send_snapshot = true;
        }
        if send_snapshot {
            send_snapshot = false;
            let snap: Vec<Quote> = resolved
                .iter()
                .zip(stamps.iter_mut())
                .filter_map(|((k, c), stamp)| {
                    let m = hub.mids.get(c)?;
                    *stamp = m.stamp;
                    Some(quote_for(&hub, k, c, &m))
                })
                .collect();
            if !f.snapshot(TopicData::Quotes(snap)).await {
                return;
            }
        } else {
            let mut newest: Option<Instant> = None;
            let changed: Vec<Quote> = resolved
                .iter()
                .zip(stamps.iter_mut())
                .filter_map(|((k, c), stamp)| {
                    let m = hub.mids.get(c).filter(|m| m.stamp > *stamp)?;
                    *stamp = m.stamp;
                    newest = newest.max(Some(m.recv));
                    Some(quote_for(&hub, k, c, &m))
                })
                .collect();
            if let Some(recv) = newest
                && !f.delta(TopicDelta::Quotes(changed), recv).await
            {
                return;
            }
        }
        let sent = Instant::now();
        tokio::select! {
            r = version.changed() => if r.is_err() { return },
            _ = f.resync.notified() => send_snapshot = true,
        }
        // Conflate: at most `hz` messages a second.
        let next = sent + period;
        if next > Instant::now() {
            tokio::time::sleep_until(next.into()).await;
        }
    }
}

/// Group levels into `tick` buckets (bids down, asks up), then keep `depth` a side.
fn shape(levels: &[Level], depth: usize, group: Option<f64>, up: bool) -> Vec<Level> {
    let Some(tick) = group.filter(|t| *t > 0.0 && t.is_finite()) else {
        return levels.iter().take(depth).copied().collect();
    };
    let mut out: Vec<Level> = Vec::with_capacity(depth);
    for l in levels {
        let b = if up { (l.px / tick).ceil() } else { (l.px / tick).floor() } * tick;
        if let Some(last) = out.last_mut().filter(|last| (last.px - b).abs() < tick * 1e-6) {
            last.sz += l.sz;
            last.n += l.n;
        } else if out.len() == depth {
            break;
        } else {
            out.push(Level { px: b, sz: l.sz, n: l.n });
        }
    }
    out
}

async fn book(mut f: Fwd, topic: Arc<BookTopic>, depth: usize, group: Option<f64>) {
    let mut rx = topic.state.subscribe();
    let mut epoch: Option<u64> = None;
    let mut status = TopicStatus::Live;
    let mut last: Option<Arc<crate::store::BookSnap>> = None;
    let mut need_snapshot = true;
    loop {
        let st = rx.borrow_and_update().clone();
        if st.status != status {
            status = st.status.clone();
            if !f.status(status.clone()).await {
                return;
            }
        }
        if epoch.is_some_and(|e| e != st.epoch) && !need_snapshot {
            if !f.resync(ResyncReason::Upstream).await {
                return;
            }
            need_snapshot = true;
        }
        epoch = Some(st.epoch);
        if let Some(b) = st.book.filter(|b| last.as_ref().is_none_or(|l| !Arc::ptr_eq(l, b))) {
            let data = Book {
                bids: shape(&b.bids, depth, group, false),
                asks: shape(&b.asks, depth, group, true),
                time_ms: b.time_ms,
            };
            let ok = if need_snapshot {
                need_snapshot = false;
                f.snapshot(TopicData::Book(data)).await
            } else {
                f.delta(TopicDelta::Book(data), b.recv).await
            };
            if !ok {
                return;
            }
            last = Some(b);
        }
        tokio::select! {
            r = rx.changed() => if r.is_err() { return },
            _ = f.resync.notified() => {
                need_snapshot = true;
                last = None;
            }
        }
    }
}

async fn trades(mut f: Fwd, topic: Arc<TradesTopic>) {
    let mut rx = topic.subscribe();
    let mut seen = 0;
    let mut snapshot = true;
    loop {
        if std::mem::take(&mut snapshot) {
            let (list, seq, status) = topic.snapshot();
            seen = seq;
            if !f.snapshot(TopicData::Trades(list)).await {
                return;
            }
            if status != TopicStatus::Live && !f.status(status).await {
                return;
            }
        }
        let ev = tokio::select! {
            ev = rx.recv() => ev,
            _ = f.resync.notified() => {
                snapshot = true;
                continue;
            }
        };
        match ev {
            Ok(ev) if ev.seq <= seen => {}
            Ok(ev) => {
                seen = ev.seq;
                let ok = match &ev.kind {
                    EventKind::Data(t) => f.delta(TopicDelta::Trades(t.clone()), ev.recv).await,
                    EventKind::Status(s) => f.status(s.clone()).await,
                    EventKind::Resync => {
                        snapshot = true;
                        f.resync(ResyncReason::Upstream).await
                    }
                };
                if !ok {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                snapshot = true;
                if !f.resync(ResyncReason::Overflow).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Drop each `Forming` bar that a later event in the batch supersedes (same `ts`, or a resync).
/// `Closed` bars and statuses always stay, in order.
pub fn coalesce(batch: Vec<Arc<Event<BarDelta>>>) -> Vec<Arc<Event<BarDelta>>> {
    let keep = |i: usize, e: &Event<BarDelta>| match &e.kind {
        EventKind::Data(BarDelta::Forming(b)) => !batch[i + 1..].iter().any(|later| match &later.kind {
            EventKind::Data(d) => d.bar().ts == b.ts,
            EventKind::Resync => true,
            EventKind::Status(_) => false,
        }),
        _ => true,
    };
    batch.iter().enumerate().filter(|(i, e)| keep(*i, e)).map(|(_, e)| e.clone()).collect()
}

async fn bars_snapshot(f: &mut Fwd, hub: &Hub, topic: &BarsTopic, history: usize) -> Option<u64> {
    let since = topic.overlay().live_since;
    let hist = if history == 0 {
        Arc::new(Vec::new())
    } else {
        match hub.history(&topic.coin, topic.tf, None, history, since.or(Some(Instant::now()))).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("history {} {}: {e:#}", topic.coin, topic.tf);
                Arc::new(Vec::new())
            }
        }
    };
    let ov = topic.overlay();
    let bars = merge_bars(hist.to_vec(), &ov.bars, history.max(ov.bars.len()));
    if !f.snapshot(TopicData::Bars(bars)).await {
        return None;
    }
    if ov.status != TopicStatus::Live && !f.status(ov.status).await {
        return None;
    }
    Some(ov.seq)
}

async fn bars(mut f: Fwd, hub: &Hub, topic: Arc<BarsTopic>, history: usize) {
    let mut rx = topic.subscribe();
    let Some(mut seen) = bars_snapshot(&mut f, hub, &topic, history).await else { return };
    loop {
        let first = tokio::select! {
            ev = rx.recv() => ev,
            _ = f.resync.notified() => {
                match bars_snapshot(&mut f, hub, &topic, history).await {
                    Some(s) => seen = s,
                    None => return,
                }
                continue;
            }
        };
        let mut batch = Vec::new();
        let mut lagged = false;
        match first {
            Ok(e) => batch.push(e),
            Err(broadcast::error::RecvError::Lagged(_)) => lagged = true,
            Err(broadcast::error::RecvError::Closed) => return,
        }
        // Whatever else is already waiting goes out together, superseded bars dropped.
        loop {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(broadcast::error::TryRecvError::Lagged(_)) => lagged = true,
                Err(_) => break,
            }
        }
        if lagged {
            if !f.resync(ResyncReason::Overflow).await {
                return;
            }
            match bars_snapshot(&mut f, hub, &topic, history).await {
                Some(s) => seen = s,
                None => return,
            }
            continue;
        }
        batch.retain(|e| e.seq > seen);
        let Some(last) = batch.last() else { continue };
        seen = last.seq;
        for e in coalesce(batch) {
            let ok = match &e.kind {
                EventKind::Data(d) => f.delta(TopicDelta::Bar(*d), e.recv).await,
                EventKind::Status(s) => f.status(s.clone()).await,
                EventKind::Resync => {
                    if !f.resync(ResyncReason::Upstream).await {
                        return;
                    }
                    // The fresh snapshot covers the rest of this batch.
                    match bars_snapshot(&mut f, hub, &topic, history).await {
                        Some(s) => seen = seen.max(s),
                        None => return,
                    }
                    break;
                }
            };
            if !ok {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tr_core::Bar;

    fn ev(seq: u64, kind: EventKind<BarDelta>) -> Arc<Event<BarDelta>> {
        Arc::new(Event { seq, recv: Instant::now(), kind })
    }

    fn b(ts: i64, c: f64) -> Bar {
        Bar::new(ts, c, c, c, c, 1.0)
    }

    fn kinds(v: &[Arc<Event<BarDelta>>]) -> Vec<String> {
        v.iter()
            .map(|e| match &e.kind {
                EventKind::Data(BarDelta::Forming(x)) => format!("F{}:{}", x.ts, x.close),
                EventKind::Data(BarDelta::Closed(x)) => format!("C{}:{}", x.ts, x.close),
                EventKind::Resync => "R".into(),
                EventKind::Status(_) => "S".into(),
            })
            .collect()
    }

    /// A slow reader that wakes up across a bar boundary still gets the closed bar's final
    /// values before the next bar, and only the latest state of each forming bar.
    #[test]
    fn slow_reader_across_a_bar_boundary() {
        let batch = vec![
            ev(1, EventKind::Data(BarDelta::Forming(b(0, 1.0)))),
            ev(2, EventKind::Data(BarDelta::Forming(b(0, 2.0)))),
            ev(3, EventKind::Data(BarDelta::Closed(b(0, 2.5)))),
            ev(4, EventKind::Data(BarDelta::Forming(b(60, 3.0)))),
            ev(5, EventKind::Status(TopicStatus::Live)),
            ev(6, EventKind::Data(BarDelta::Forming(b(60, 4.0)))),
        ];
        assert_eq!(kinds(&coalesce(batch)), ["C0:2.5", "S", "F60:4"]);
        let batch = vec![ev(1, EventKind::Data(BarDelta::Forming(b(0, 1.0)))), ev(2, EventKind::Resync)];
        assert_eq!(kinds(&coalesce(batch)), ["R"]);
        let batch = vec![ev(1, EventKind::Data(BarDelta::Forming(b(0, 1.0))))];
        assert_eq!(kinds(&coalesce(batch)), ["F0:1"]);
    }

    #[test]
    fn book_grouping_and_depth() {
        let l = |px: f64, sz: f64| Level { px, sz, n: 1 };
        let bids = [l(100.4, 1.0), l(100.1, 2.0), l(99.7, 1.0), l(99.2, 1.0)];
        let asks = [l(100.6, 1.0), l(100.9, 3.0), l(101.2, 1.0)];
        assert_eq!(shape(&bids, 2, None, false), bids[..2]);
        let g = shape(&bids, 10, Some(0.5), false);
        assert_eq!(
            g.iter().map(|x| (x.px, x.sz, x.n)).collect::<Vec<_>>(),
            [(100.0, 3.0, 2), (99.5, 1.0, 1), (99.0, 1.0, 1)]
        );
        assert_eq!(shape(&bids, 1, Some(0.5), false).len(), 1);
        let g = shape(&asks, 10, Some(0.5), true);
        assert_eq!(g.iter().map(|x| (x.px, x.sz)).collect::<Vec<_>>(), [(101.0, 4.0), (101.5, 1.0)]);
        assert_eq!(shape(&asks, 5, Some(f64::NAN), true).len(), 3, "bad ticks mean no grouping");
    }
}
