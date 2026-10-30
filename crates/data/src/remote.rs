//! A client for the Terminal data server (`tr-gateway`), and [`RemoteMarkets`], which serves
//! crypto markets through it (docs/architecture/server.md §4.6).
//!
//! [`Client`] keeps one connection per server, shared by every subscription and request. It
//! reconnects with backoff, resubscribes everything (each subscription then gets a fresh
//! `Snapshot`), checks each subscription's `seq` and asks for a `Resync` on a gap.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tr_core::{AssetClass, Bar, Timeframe};
use tr_proto::{
    ApiError, BarDelta, ClientInfo, ClientMsg, Encoding, ErrorCode, Frame, MarketKey, PROTOCOL_VERSION, ReqId,
    ResyncReason, Rpc, RpcResult, ServerMsg, SubId, TopicData, TopicDelta, TopicReq, TopicStatus,
};

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
    Up { session: u64 },
    Down(String),
}

enum Cmd {
    Subscribe { id: SubId, topic: TopicReq, tx: mpsc::Sender<Update> },
    Unsubscribe(SubId),
    Request { id: ReqId, req: Rpc, reply: oneshot::Sender<Result<RpcResult, ApiError>> },
}

/// A shared connection to one server. Cheap to clone. Create it inside a tokio runtime.
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
        Frame::Binary(b) => Message::Binary(b.into()),
        Frame::Text(t) => Message::Text(t.into()),
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
        tokio::spawn(run(normalize_url(url), encoding, info, rx, state_tx));
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
        tokio::time::timeout(timeout, up).await.is_ok()
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
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
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

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn hello(url: &str, enc: Encoding, info: &ClientInfo) -> Result<(Ws, u64)> {
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(url))
        .await
        .context("connect timed out")?
        .context("connect")?;
    let hello =
        ClientMsg::Hello { version: PROTOCOL_VERSION, encoding: enc, client: info.clone(), token: None, resume: None };
    ws.send(to_message(tr_proto::encode(&hello, enc)?)).await?;
    let welcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(m) = ws.next().await {
            match decode(&m?) {
                Some(Ok(ServerMsg::Welcome { session, .. })) => return Ok(session.0),
                Some(Ok(ServerMsg::Error { code, message })) => bail!("{code:?}: {message}"),
                _ => {}
            }
        }
        bail!("closed before Welcome")
    })
    .await
    .context("no Welcome")??;
    Ok((ws, welcome))
}

fn decode(m: &Message) -> Option<Result<ServerMsg, tr_proto::CodecError>> {
    match m {
        Message::Binary(b) => Some(tr_proto::decode_binary(b)),
        Message::Text(t) => Some(tr_proto::decode_text(t.as_str())),
        _ => None,
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
    loop {
        state.send_replace(ConnState::Connecting);
        match hello(&url, enc, &info).await {
            Ok((ws, session)) => {
                state.send_replace(ConnState::Up { session });
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
        let sleep = tokio::time::sleep(backoff);
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
    ws: Ws,
    enc: Encoding,
    subs: &mut HashMap<SubId, SubState>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
) -> bool {
    let (mut tx, mut rx) = ws.split();
    let mut pending: HashMap<ReqId, oneshot::Sender<Result<RpcResult, ApiError>>> = HashMap::new();
    let mut queue: Vec<ClientMsg> =
        subs.iter().map(|(id, s)| ClientMsg::Subscribe { id: *id, topic: s.topic.clone() }).collect();
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    loop {
        for m in queue.drain(..) {
            let Ok(f) = tr_proto::encode(&m, enc) else { continue };
            if tx.send(to_message(f)).await.is_err() {
                return true;
            }
        }
        tokio::select! {
            m = rx.next() => {
                let received = SystemTime::now();
                let Some(Ok(m)) = m else { return true };
                match decode(&m) {
                    Some(Ok(msg)) => route(msg, received, subs, &mut pending, &mut queue),
                    Some(Err(e)) => tracing::debug!("terminal server sent an unreadable frame: {e}"),
                    None => {}
                }
            }
            cmd = cmds.recv() => match cmd {
                None => {
                    let _ = tx.close().await;
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
        }
    }

    fn client(&self) -> &Client {
        self.client.get_or_init(|| Client::connect(&self.url, Encoding::Postcard, "terminal-desktop"))
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
        let quotes = tokio::time::timeout(Duration::from_secs(5), async {
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
        let got = tokio::time::timeout(Duration::from_secs(3), latest.wait_for(Option::is_some))
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
    let mut check = tokio::time::interval(Duration::from_secs(5));
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
async fn backfill(client: &Client, key: &MarketKey, tf: Timeframe, from: i64) -> Result<Vec<Bar>, ApiError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(from);
    let limit = ((now - from).max(0) / tf.seconds() + 2).min(5000) as u32;
    match client.request(Rpc::History { key: key.clone(), tf, end: None, limit }).await? {
        RpcResult::History(bars) => Ok(bars),
        _ => Err(ApiError { code: ErrorCode::Internal, message: "unexpected reply".into() }),
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
            return self.inner.live(symbol, tf);
        }
        let (tx, rx) = mpsc::channel(256);
        let client = self.client().clone();
        let key = MarketKey::new(symbol);
        // The worker already holds the history; the live stream only needs what's new.
        let mut sub = client.subscribe(TopicReq::Bars { key: key.clone(), tf, history: 0 });
        rt::spawn(async move {
            // The newest bar sent on, and whether the stream broke since.
            let mut last: Option<i64> = None;
            let mut broken = false;
            while let Some(u) = sub.next().await {
                let mut bars = match u.event {
                    Event::Resync(_) | Event::Reconnecting => {
                        broken = true;
                        continue;
                    }
                    Event::Snapshot(TopicData::Bars(b)) => b,
                    // A `Closed` bar carries the bar's final values; merging it into the forming
                    // bar the worker holds gives the same bar (the server's forming bars are
                    // full states, never partial ones).
                    Event::Delta(TopicDelta::Bar(BarDelta::Forming(b) | BarDelta::Closed(b))) => vec![b],
                    _ => continue,
                };
                // After an outage the stream picks up at the current bar. Fetch the bars that
                // closed in between first, so the chart has no hole.
                if std::mem::take(&mut broken)
                    && let Some(from) = last
                {
                    match backfill(&client, &key, tf, from).await {
                        Ok(mut missed) => {
                            missed.retain(|b| b.ts >= from);
                            missed.extend(bars);
                            bars = missed;
                        }
                        Err(e) => tracing::debug!("terminal server, backfilling {key}: {e}"),
                    }
                }
                for b in bars {
                    last = Some(last.map_or(b.ts, |l| l.max(b.ts)));
                    if tx.send(b).await.is_err() {
                        return;
                    }
                }
            }
        });
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
