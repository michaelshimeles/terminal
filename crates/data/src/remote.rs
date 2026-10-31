//! A client for the Terminal data server (`tr-gateway`), and [`RemoteMarkets`], which serves
//! crypto markets through it (docs/architecture/server.md §4.6).
//!
//! [`Client`] keeps one connection per server, shared by every subscription and request. It
//! reconnects with backoff, resubscribes everything (each subscription then gets a fresh
//! `Snapshot`), checks each subscription's `seq` and asks for a `Resync` on a gap. It runs on
//! either target: tokio on the desktop, the page's event loop in the browser build.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};
use tr_core::{AssetClass, Bar, Timeframe};
use tr_proto::{
    ApiError, BarDelta, ClientInfo, ClientMsg, Encoding, ErrorCode, Frame, MarketKey, PROTOCOL_VERSION, ReqId,
    ResyncReason, Rpc, RpcResult, ServerMsg, SubId, TopicData, TopicDelta, TopicReq, TopicStatus,
};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use crate::ws::{Message, Socket};
use crate::{LiveFeed, MarketData, NewsItem, OptionChain, OrderBook, Quote, SearchHit, YieldCurves, rt, security};

/// What a subscription receives.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Snapshot(TopicData),
    Delta(TopicDelta),
    Status(TopicStatus),
    /// The server dropped deltas; a `Snapshot` follows.
    Resync(ResyncReason),
    /// The connection dropped. The client resubscribes when it's back, and a `Snapshot` follows.
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// When the frame arrived (wall clock), for latency measurements.
    pub received: SystemTime,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    /// Connected. The server numbers the `session`; `connection` counts this client's connections
    /// from 1.
    Up {
        session: u64,
        connection: u64,
    },
    Down(String),
}

enum Cmd {
    Subscribe { id: SubId, topic: TopicReq, tx: mpsc::Sender<Update> },
    Unsubscribe(SubId),
    Request { id: ReqId, req: Rpc, reply: oneshot::Sender<Result<RpcResult, ApiError>> },
}

/// A shared connection to one server. Cheap to clone. On the desktop, create it inside a tokio
/// runtime.
#[derive(Clone)]
pub struct Client {
    cmd: mpsc::UnboundedSender<Cmd>,
    next_id: Arc<AtomicU32>,
    state: watch::Receiver<ConnState>,
}

/// A live subscription. Dropping it unsubscribes.
pub struct Subscription {
    id: SubId,
    rx: mpsc::Receiver<Update>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Subscription {
    pub fn id(&self) -> SubId {
        self.id
    }

    /// The next update; `None` once the client is gone.
    pub async fn next(&mut self) -> Option<Update> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Unsubscribe(self.id));
    }
}

/// `ws://host:port` and `wss://host` mean the server's `/ws` path.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((_, rest)) if !rest.contains('/') => format!("{url}/ws"),
        Some((_, rest)) if rest.ends_with('/') && rest.matches('/').count() == 1 => format!("{url}ws"),
        _ => url.to_string(),
    }
}

fn to_message(f: Frame) -> Message {
    match f {
        Frame::Binary(b) => Message::Binary(b),
        Frame::Text(t) => Message::Text(t),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Client {
    /// Start connecting to `url` (`ws://` or `wss://`) in the background.
    pub fn connect(url: &str, encoding: Encoding, name: &str) -> Client {
        let (cmd, rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(ConnState::Connecting);
        let info = ClientInfo { name: name.into(), version: env!("CARGO_PKG_VERSION").into() };
        rt::spawn(run(normalize_url(url), encoding, info, rx, state_tx));
        Client { cmd, next_id: Arc::new(AtomicU32::new(1)), state }
    }

    pub fn state(&self) -> ConnState {
        self.state.borrow().clone()
    }

    /// Wait up to `timeout` for the connection to be up.
    pub async fn wait_up(&self, timeout: Duration) -> bool {
        let mut st = self.state.clone();
        let up = async {
            loop {
                if matches!(*st.borrow_and_update(), ConnState::Up { .. }) {
                    return;
                }
                if st.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        };
        rt::timeout(timeout, up).await.is_ok()
    }

    pub fn subscribe(&self, topic: TopicReq) -> Subscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(1024);
        let _ = self.cmd.send(Cmd::Subscribe { id, topic, tx });
        Subscription { id, rx, cmd: self.cmd.clone() }
    }

    /// A one-shot request. Fails fast if the server isn't reachable.
    pub async fn request(&self, req: Rpc) -> Result<RpcResult, ApiError> {
        let err = |m: &str| ApiError { code: ErrorCode::Upstream, message: m.into() };
        if !self.wait_up(Duration::from_secs(3)).await {
            return Err(err("the Terminal server is not reachable"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::Request { id, req, reply }).map_err(|_| err("client stopped"))?;
        match rt::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(err("connection lost")),
            Err(_) => Err(err("request timed out")),
        }
    }
}

struct SubState {
    topic: TopicReq,
    tx: mpsc::Sender<Update>,
    /// The last `seq` delivered, while in sync.
    seq: Option<u64>,
    /// When we last asked for a resync, so a stuck reader can't make us ask nonstop.
    asked: Option<Instant>,
}

async fn hello(url: &str, enc: Encoding, info: &ClientInfo) -> Result<(Socket, u64)> {
    let mut ws = rt::timeout(Duration::from_secs(10), Socket::connect(url))
        .await
        .context("connect timed out")?
        .context("connect")?;
    let hello =
        ClientMsg::Hello { version: PROTOCOL_VERSION, encoding: enc, client: info.clone(), token: None, resume: None };
    ws.send_message(to_message(tr_proto::encode(&hello, enc)?)).await?;
    let welcome = rt::timeout(Duration::from_secs(10), async {
        while let Some(m) = ws.next_message().await {
            match decode(&m?) {
                Ok(ServerMsg::Welcome { session, .. }) => return Ok(session.0),
                Ok(ServerMsg::Error { code, message }) => bail!("{code:?}: {message}"),
                _ => {}
            }
        }
        bail!("closed before Welcome")
    })
    .await
    .context("no Welcome")??;
    Ok((ws, welcome))
}

fn decode(m: &Message) -> Result<ServerMsg, tr_proto::CodecError> {
    match m {
        Message::Binary(b) => tr_proto::decode_binary(b),
        Message::Text(t) => tr_proto::decode_text(t),
    }
}

async fn run(
    url: String,
    enc: Encoding,
    info: ClientInfo,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    state: watch::Sender<ConnState>,
) {
    let mut subs: HashMap<SubId, SubState> = HashMap::new();
    let mut backoff = Duration::from_millis(500);
    let mut connection = 0;
    loop {
        state.send_replace(ConnState::Connecting);
        match hello(&url, enc, &info).await {
            Ok((ws, session)) => {
                connection += 1;
                state.send_replace(ConnState::Up { session, connection });
                backoff = Duration::from_millis(500);
                if !connected(ws, enc, &mut subs, &mut cmds).await {
                    return;
                }
                state.send_replace(ConnState::Down("connection lost".into()));
                for s in subs.values_mut() {
                    s.seq = None;
                    let _ = s.tx.try_send(Update { received: SystemTime::now(), event: Event::Reconnecting });
                }
            }
            Err(e) => {
                tracing::debug!("terminal server: {e:#}");
                state.send_replace(ConnState::Down(format!("{e:#}")));
            }
        }
        // Back off, still taking commands: subscriptions wait for the next connection,
        // requests fail now.
        let sleep = rt::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmds.recv() => match cmd {
                    None => return,
                    Some(Cmd::Subscribe { id, topic, tx }) => {
                        subs.insert(id, SubState { topic, tx, seq: None, asked: None });
                    }
                    Some(Cmd::Unsubscribe(id)) => {
                        subs.remove(&id);
                    }
                    Some(Cmd::Request { reply, .. }) => {
                        let _ = reply.send(Err(ApiError { code: ErrorCode::Upstream, message: "not connected".into() }));
                    }
                },
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Serve one connection. Returns false once every handle to the client is gone.
async fn connected(
    mut ws: Socket,
    enc: Encoding,
    subs: &mut HashMap<SubId, SubState>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
) -> bool {
    let mut pending: HashMap<ReqId, oneshot::Sender<Result<RpcResult, ApiError>>> = HashMap::new();
    let mut queue: Vec<ClientMsg> =
        subs.iter().map(|(id, s)| ClientMsg::Subscribe { id: *id, topic: s.topic.clone() }).collect();
    let mut ping = rt::interval(Duration::from_secs(15));
    loop {
        for m in queue.drain(..) {
            let Ok(f) = tr_proto::encode(&m, enc) else { continue };
            if ws.send_message(to_message(f)).await.is_err() {
                return true;
            }
        }
        tokio::select! {
            m = ws.next_message() => {
                let received = SystemTime::now();
                let Some(Ok(m)) = m else { return true };
                match decode(&m) {
                    Ok(msg) => route(msg, received, subs, &mut pending, &mut queue),
                    Err(e) => tracing::debug!("terminal server sent an unreadable frame: {e}"),
                }
            }
            cmd = cmds.recv() => match cmd {
                None => {
                    ws.close().await;
                    return false;
                }
                Some(Cmd::Subscribe { id, topic, tx: sub }) => {
                    queue.push(ClientMsg::Subscribe { id, topic: topic.clone() });
                    subs.insert(id, SubState { topic, tx: sub, seq: None, asked: None });
                }
                Some(Cmd::Unsubscribe(id)) => {
                    if subs.remove(&id).is_some() {
                        queue.push(ClientMsg::Unsubscribe { id });
                    }
                }
                Some(Cmd::Request { id, req, reply }) => {
                    pending.insert(id, reply);
                    queue.push(ClientMsg::Request { id, req });
                }
            },
            _ = ping.tick() => queue.push(ClientMsg::Ping { t: now_ms() }),
        }
    }
}

fn route(
    msg: ServerMsg,
    received: SystemTime,
    subs: &mut HashMap<SubId, SubState>,
    pending: &mut HashMap<ReqId, oneshot::Sender<Result<RpcResult, ApiError>>>,
    queue: &mut Vec<ClientMsg>,
) {
    let mut deliver = |id: SubId, seq: Option<u64>, event: Event| {
        let Some(s) = subs.get_mut(&id) else { return };
        match (&event, seq) {
            (Event::Snapshot(_), Some(n)) => s.seq = Some(n),
            (Event::Delta(_), Some(n)) if s.seq.is_some_and(|last| last + 1 == n) => s.seq = Some(n),
            (Event::Delta(_), Some(_)) => {
                // A gap, or a delta before any snapshot: drop it and ask for a fresh snapshot.
                if s.seq.take().is_some() || s.asked.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                    s.asked = Some(Instant::now());
                    queue.push(ClientMsg::Resync { id });
                }
                return;
            }
            (Event::Resync(_), _) => s.seq = None,
            _ => {}
        }
        if s.tx.try_send(Update { received, event }).is_err() && s.seq.take().is_some() {
            // The reader is behind: what it gets next must be a fresh snapshot.
            s.asked = Some(Instant::now());
            queue.push(ClientMsg::Resync { id });
        }
    };
    match msg {
        ServerMsg::Snapshot { id, seq, data } => deliver(id, Some(seq), Event::Snapshot(data)),
        ServerMsg::Delta { id, seq, data } => deliver(id, Some(seq), Event::Delta(data)),
        ServerMsg::Resync { id, reason } => deliver(id, None, Event::Resync(reason)),
        ServerMsg::Status { id, status } => deliver(id, None, Event::Status(status)),
        ServerMsg::Reply { id, result } => {
            if let Some(r) = pending.remove(&id) {
                let _ = r.send(result);
            }
        }
        ServerMsg::Error { code, message } => tracing::warn!("terminal server: {code:?}: {message}"),
        ServerMsg::Welcome { .. } | ServerMsg::Pong { .. } => {}
    }
}

// ---- RemoteMarkets ----

/// What the order book pane shows as the source of a book that came through the server.
pub const BOOK_SOURCE: &str = "Hyperliquid perp · Terminal server";

struct BookFeed {
    latest: watch::Receiver<Option<Result<OrderBook, String>>>,
    used: Arc<Mutex<Instant>>,
}

/// Crypto markets (`BTCUSD Curncy`) through the Terminal server, which backs them with
/// Hyperliquid perps; everything else, and anything the server doesn't list, from `inner`.
/// The connection opens on first use, inside the worker's runtime.
pub struct RemoteMarkets<P> {
    url: String,
    client: OnceLock<Client>,
    inner: Arc<P>,
    name: String,
    /// Markets the server said it doesn't have.
    unknown: Mutex<HashSet<String>>,
    /// Markets whose latest history came from the server, so paging and the live feed must too.
    served: Mutex<HashSet<String>>,
    /// Markets the server serves whose latest history came from `inner` instead, with the
    /// connection that was up then (0 for none). A newer connection moves their charts back.
    fallbacks: Mutex<HashMap<String, u64>>,
    books: Arc<Mutex<HashMap<String, BookFeed>>>,
    sparks: Mutex<HashMap<String, (Instant, Vec<f64>)>>,
}

impl<P: MarketData + 'static> RemoteMarkets<P> {
    pub fn new(url: &str, inner: P) -> Self {
        let name = format!("server+{}", inner.name());
        Self {
            url: url.into(),
            client: OnceLock::new(),
            inner: Arc::new(inner),
            name,
            unknown: Mutex::default(),
            served: Mutex::default(),
            books: Arc::default(),
            sparks: Mutex::default(),
            fallbacks: Mutex::default(),
        }
    }

    fn client(&self) -> &Client {
        let name = if cfg!(target_arch = "wasm32") { "terminal-web" } else { "terminal-desktop" };
        self.client.get_or_init(|| Client::connect(&self.url, Encoding::Postcard, name))
    }

    fn routed(&self, symbol: &str) -> bool {
        security(symbol).class == AssetClass::Crypto && !self.unknown.lock().expect("unknown lock").contains(symbol)
    }

    fn served(&self, symbol: &str) -> bool {
        self.served.lock().expect("served lock").contains(symbol)
    }

    fn note(&self, symbol: &str, e: &ApiError) {
        if e.code == ErrorCode::UnknownMarket {
            self.unknown.lock().expect("unknown lock").insert(symbol.to_string());
        }
        tracing::debug!("terminal server, {symbol}: {e}");
    }

    async fn server_history(
        &self,
        symbol: &str,
        tf: Timeframe,
        end: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Bar>, ApiError> {
        let req = Rpc::History { key: MarketKey::new(symbol), tf, end, limit: limit.min(5000) as u32 };
        match self.client().request(req).await? {
            RpcResult::History(bars) => Ok(bars),
            _ => Err(ApiError { code: ErrorCode::Internal, message: "unexpected reply".into() }),
        }
    }

    /// Daily closes for the watchlist sparkline, kept for ten minutes.
    async fn spark(&self, symbol: &str) -> Vec<f64> {
        if let Some((at, v)) = self.sparks.lock().expect("sparks lock").get(symbol)
            && at.elapsed() < Duration::from_secs(600)
        {
            return v.clone();
        }
        let v: Vec<f64> = match self.server_history(symbol, Timeframe::D1, None, 8).await {
            Ok(b) => b.iter().map(|b| b.close).collect(),
            Err(_) => return Vec::new(),
        };
        self.sparks.lock().expect("sparks lock").insert(symbol.to_string(), (Instant::now(), v.clone()));
        v
    }

    async fn server_quotes(&self, keys: &[String]) -> Result<Vec<Quote>> {
        let client = self.client();
        if !client.wait_up(Duration::from_secs(3)).await {
            bail!("the Terminal server is not reachable");
        }
        let topic = TopicReq::Quotes { keys: keys.iter().map(MarketKey::new).collect(), max_hz: 1 };
        let mut sub = client.subscribe(topic);
        let quotes = rt::timeout(Duration::from_secs(5), async {
            while let Some(u) = sub.next().await {
                match u.event {
                    Event::Snapshot(TopicData::Quotes(q)) => return Ok(q),
                    Event::Status(TopicStatus::Unavailable { detail, .. }) => bail!("{detail}"),
                    _ => {}
                }
            }
            bail!("connection closed")
        })
        .await
        .context("no quotes snapshot")??;
        drop(sub);
        let sparks = futures::future::join_all(quotes.iter().map(|q| self.spark(q.key.as_str()))).await;
        Ok(quotes
            .into_iter()
            .zip(sparks)
            .map(|(q, spark)| Quote {
                symbol: q.key.0,
                last: q.px,
                prev_close: q.prev_day.unwrap_or(q.px),
                ts: q.ts_ms / 1000,
                spark,
            })
            .collect())
    }

    async fn server_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        let mut latest = {
            let mut books = self.books.lock().expect("books lock");
            match books.get(symbol) {
                Some(f) => {
                    *f.used.lock().expect("used lock") = Instant::now();
                    f.latest.clone()
                }
                None => {
                    let (tx, latest) = watch::channel(None);
                    let used = Arc::new(Mutex::new(Instant::now()));
                    let sub =
                        self.client().subscribe(TopicReq::Book { key: MarketKey::new(symbol), depth: 20, group: None });
                    rt::spawn(book_feed(symbol.to_string(), sub, tx, used.clone(), self.books.clone()));
                    books.insert(symbol.to_string(), BookFeed { latest: latest.clone(), used });
                    latest
                }
            }
        };
        let got = rt::timeout(Duration::from_secs(3), latest.wait_for(Option::is_some))
            .await
            .context("no book from the Terminal server")?
            .map_err(|_| anyhow!("book feed stopped"))?
            .clone();
        let mut book = got.expect("waited for Some").map_err(|e| anyhow!(e))?;
        book.bids.truncate(depth);
        book.asks.truncate(depth);
        Ok(book)
    }
}

/// Keep one book subscription per market while the pane keeps asking, and drop it 30 s after
/// the last ask.
async fn book_feed(
    symbol: String,
    mut sub: Subscription,
    tx: watch::Sender<Option<Result<OrderBook, String>>>,
    used: Arc<Mutex<Instant>>,
    books: Arc<Mutex<HashMap<String, BookFeed>>>,
) {
    let mut check = rt::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            u = sub.next() => {
                let Some(u) = u else { break };
                let book = match u.event {
                    Event::Snapshot(TopicData::Book(b)) | Event::Delta(TopicDelta::Book(b)) => b,
                    Event::Status(TopicStatus::Unavailable { detail, .. }) => {
                        tx.send_replace(Some(Err(detail)));
                        continue;
                    }
                    // The last book goes stale while the connection is down; the next snapshot replaces it.
                    Event::Reconnecting => {
                        tx.send_replace(Some(Err("the Terminal server is not reachable".into())));
                        continue;
                    }
                    _ => continue,
                };
                let level = |l: &tr_proto::Level| (l.px, l.sz);
                tx.send_replace(Some(Ok(OrderBook {
                    symbol: symbol.clone(),
                    bids: book.bids.iter().map(level).collect(),
                    asks: book.asks.iter().map(level).collect(),
                    source: BOOK_SOURCE.into(),
                    simulated: false,
                })));
            }
            _ = check.tick() => if used.lock().expect("used lock").elapsed() > Duration::from_secs(30) {
                break;
            },
        }
    }
    books.lock().expect("books lock").remove(&symbol);
}

/// The bars from `from` (Unix seconds, the bar open) up to now.
async fn history_since(client: &Client, key: &MarketKey, tf: Timeframe, from: i64) -> Result<Vec<Bar>, ApiError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(from);
    let limit = ((now - from).max(0) / tf.seconds() + 2).min(5000) as u32;
    match client.request(Rpc::History { key: key.clone(), tf, end: None, limit }).await? {
        RpcResult::History(bars) => Ok(bars),
        _ => Err(ApiError { code: ErrorCode::Internal, message: "unexpected reply".into() }),
    }
}

type Fetch = std::pin::Pin<Box<dyn Future<Output = Result<Vec<Bar>, ApiError>> + Send>>;

/// How long a chart's live feed from the server waits for a dropped connection to come back
/// before it ends, so the worker reloads the chart from the fallback.
const OUTAGE_GRACE: Duration = Duration::from_secs(10);

/// The server's bar stream, onto the worker's live feed. Live bars go straight through. After an
/// outage the stream picks up at the current bar, so the bars from the last one sent up to it
/// come from history: one request at a time per feed, retried with backoff, every outstanding
/// gap merged into one range. The worker slots those bars into the gap. A connection that stays
/// down past `OUTAGE_GRACE` ends the feed instead. Everything stops when the worker drops the feed.
async fn live_bridge(client: Client, key: MarketKey, tf: Timeframe, mut sub: Subscription, tx: mpsc::Sender<Bar>) {
    // The newest bar sent on, whether the stream broke since, and when the connection dropped.
    let mut last: Option<i64> = None;
    let mut broken = false;
    let mut away: Option<Instant> = None;
    // Bars still to fetch, `[from, until)`, and the range the request in flight covers.
    let mut pending: Option<(i64, i64)> = None;
    let mut fetching: Option<((i64, i64), Fetch)> = None;
    let mut retry_at: Option<Instant> = None;
    let mut wait = Duration::from_secs(1);
    let union = |a: Option<(i64, i64)>, b: (i64, i64)| Some(a.map_or(b, |a| (a.0.min(b.0), a.1.max(b.1))));
    loop {
        if fetching.is_none()
            && retry_at.is_none()
            && let Some(range) = pending.take()
        {
            let (client, key) = (client.clone(), key.clone());
            fetching = Some((range, Box::pin(async move { history_since(&client, &key, tf, range.0).await })));
        }
        let retry_in = retry_at.map(|t| t.saturating_duration_since(Instant::now()));
        let give_up_in = away.map(|t| OUTAGE_GRACE.saturating_sub(t.elapsed()));
        tokio::select! {
            u = sub.next() => {
                let Some(u) = u else { return };
                let bars = match u.event {
                    Event::Resync(_) => {
                        broken = true;
                        continue;
                    }
                    Event::Reconnecting => {
                        broken = true;
                        away.get_or_insert_with(Instant::now);
                        continue;
                    }
                    Event::Snapshot(TopicData::Bars(b)) => {
                        away = None;
                        b
                    }
                    // A `Closed` bar carries the bar's final values; merging it into the forming
                    // bar the worker holds gives the same bar (the server's forming bars are
                    // full states, never partial ones).
                    Event::Delta(TopicDelta::Bar(BarDelta::Forming(b) | BarDelta::Closed(b))) => vec![b],
                    _ => continue,
                };
                let Some(resume) = bars.iter().map(|b| b.ts).min() else { continue };
                // `from` itself is included: the worker holds only its partial state.
                if std::mem::take(&mut broken)
                    && let Some(from) = last.filter(|&f| f < resume)
                {
                    pending = union(pending, (from, resume));
                }
                for b in bars {
                    last = Some(last.map_or(b.ts, |l| l.max(b.ts)));
                    if tx.send(b).await.is_err() {
                        return;
                    }
                }
            }
            r = async { fetching.as_mut().expect("guarded").1.as_mut().await }, if fetching.is_some() => {
                let (range, _) = fetching.take().expect("guarded");
                match r {
                    Ok(bars) => {
                        wait = Duration::from_secs(1);
                        for b in bars.into_iter().filter(|b| b.ts >= range.0 && b.ts < range.1) {
                            if tx.send(b).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("terminal server, backfilling {key}: {e}; retrying in {wait:?}");
                        pending = union(pending, range);
                        retry_at = Some(Instant::now() + wait);
                        wait = (wait * 2).min(Duration::from_secs(30));
                    }
                }
            }
            _ = async { rt::sleep(retry_in.expect("guarded")).await }, if retry_in.is_some() => {
                retry_at = None;
            }
            _ = async { rt::sleep(give_up_in.expect("guarded")).await }, if give_up_in.is_some() => {
                tracing::debug!("terminal server still unreachable; ending the {key} {tf} feed");
                return;
            }
            _ = tx.closed() => return,
        }
    }
}

/// A fallback's live bars for a market the server serves, passed on until a connection newer than
/// `after` is up, one the chart didn't fall back on. Then the feed ends, so the worker reloads the
/// chart from the server.
async fn until_server_up(mut state: watch::Receiver<ConnState>, after: u64, mut feed: LiveFeed, tx: mpsc::Sender<Bar>) {
    let newer = |s: &ConnState| matches!(s, ConnState::Up { connection, .. } if *connection > after);
    if newer(&state.borrow_and_update()) {
        return;
    }
    let mut watching = true;
    loop {
        tokio::select! {
            b = feed.recv() => {
                let Some(b) = b else { return };
                if tx.send(b).await.is_err() {
                    return;
                }
            }
            changed = state.changed(), if watching => {
                if changed.is_err() {
                    watching = false;
                    continue;
                }
                if newer(&state.borrow_and_update()) {
                    return;
                }
            }
            _ = tx.closed() => return,
        }
    }
}

#[async_trait]
impl<P: MarketData + 'static> MarketData for RemoteMarkets<P> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        // Older pages come from wherever the bars already shown came from, so a chart never
        // splices one venue's bars onto another's.
        if end.is_some() {
            if self.served(symbol) {
                return self.server_history(symbol, tf, end, limit).await.map_err(|e| anyhow!("{e}"));
            }
            return self.inner.history(symbol, tf, end, limit).await;
        }
        if self.routed(symbol) {
            match self.server_history(symbol, tf, end, limit).await {
                Ok(bars) if !bars.is_empty() => {
                    self.served.lock().expect("served lock").insert(symbol.to_string());
                    return Ok(bars);
                }
                Ok(_) => {}
                Err(e) => self.note(symbol, &e),
            }
            let connection = match self.client().state() {
                ConnState::Up { connection, .. } => connection,
                _ => 0,
            };
            self.fallbacks.lock().expect("fallbacks lock").insert(symbol.to_string(), connection);
        }
        self.served.lock().expect("served lock").remove(symbol);
        self.inner.history(symbol, tf, end, limit).await
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let (routed, mut other): (Vec<String>, Vec<String>) = symbols.iter().cloned().partition(|s| self.routed(s));
        let mut out = Vec::new();
        if !routed.is_empty() {
            match self.server_quotes(&routed).await {
                Ok(q) => {
                    other.extend(routed.iter().filter(|s| !q.iter().any(|x| &x.symbol == *s)).cloned());
                    out.extend(q);
                }
                Err(e) => {
                    tracing::debug!("terminal server quotes: {e:#}");
                    other.extend(routed);
                }
            }
        }
        if !other.is_empty() {
            match self.inner.quotes(&other).await {
                Ok(q) => out.extend(q),
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => tracing::warn!("quotes: {e:#}"),
            }
        }
        Ok(out)
    }

    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        if !self.served(symbol) {
            let feed = self.inner.live(symbol, tf)?;
            if !self.routed(symbol) {
                return Some(feed);
            }
            let after = self.fallbacks.lock().expect("fallbacks lock").get(symbol).copied().unwrap_or(0);
            let (tx, rx) = mpsc::channel(256);
            rt::spawn(until_server_up(self.client().state.clone(), after, feed, tx));
            return Some(rx);
        }
        let (tx, rx) = mpsc::channel(256);
        let client = self.client().clone();
        let key = MarketKey::new(symbol);
        // The worker already holds the history; the live stream only needs what's new.
        let sub = client.subscribe(TopicReq::Bars { key: key.clone(), tf, history: 0 });
        rt::spawn(live_bridge(client, key, tf, sub, tx));
        Some(rx)
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.inner.search(query).await
    }

    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        self.inner.news(symbol).await
    }

    async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<OptionChain> {
        self.inner.options(symbol, expiration).await
    }

    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        if self.routed(symbol) {
            match self.server_book(symbol, depth).await {
                Ok(b) => return Ok(b),
                Err(e) => tracing::debug!("terminal server book {symbol}: {e:#}"),
            }
        }
        self.inner.order_book(symbol, depth).await
    }

    async fn yield_curves(&self) -> Result<YieldCurves> {
        self.inner.yield_curves().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_default_to_the_ws_path() {
        assert_eq!(normalize_url("ws://localhost:8787"), "ws://localhost:8787/ws");
        assert_eq!(normalize_url("wss://data.example.com/"), "wss://data.example.com/ws");
        assert_eq!(normalize_url("wss://data.example.com/ws"), "wss://data.example.com/ws");
        assert_eq!(normalize_url(" ws://h:1/custom/path "), "ws://h:1/custom/path");
    }

    #[test]
    fn gaps_ask_for_a_resync() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut subs = HashMap::from([(
            7,
            SubState { topic: TopicReq::Trades { key: MarketKey::new("BTC") }, tx, seq: None, asked: None },
        )]);
        let mut pending = HashMap::new();
        let mut queue = Vec::new();
        let t = SystemTime::now();
        let delta = |seq| ServerMsg::Delta { id: 7, seq, data: TopicDelta::Trades(vec![]) };
        // A delta before any snapshot is dropped and asks for one.
        route(delta(1), t, &mut subs, &mut pending, &mut queue);
        assert_eq!(queue, [ClientMsg::Resync { id: 7 }]);
        assert!(rx.try_recv().is_err());
        queue.clear();
        route(
            ServerMsg::Snapshot { id: 7, seq: 10, data: TopicData::Trades(vec![]) },
            t,
            &mut subs,
            &mut pending,
            &mut queue,
        );
        route(delta(11), t, &mut subs, &mut pending, &mut queue);
        route(delta(13), t, &mut subs, &mut pending, &mut queue);
        assert_eq!(queue, [ClientMsg::Resync { id: 7 }], "11 follows 10; 13 skips 12");
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).map(|u| u.event).collect();
        assert!(matches!(got[..], [Event::Snapshot(_), Event::Delta(_)]));
    }
}
