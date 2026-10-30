//! The server's upstream connections to Hyperliquid's public websocket, shared by every client.
//!
//! Hyperliquid allows 10 websocket connections, 30 new connections a minute, 1000 subscriptions
//! and 2000 sent messages a minute per IP (RL in docs/architecture/server.md). The pool keeps a
//! budget below each and refuses a subscription past it; it never works around a limit.
//!
//! Permanent subscriptions (every dex's mids) get a connection of their own. Hyperliquid closes
//! a connection outright when it is asked for a coin it doesn't list, and that must not take
//! the mids feed down with it.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::wire::{self, Incoming, SubKey};
use crate::limit::WindowLimiter;
use crate::metrics::Metrics;
use crate::provider::{SubscribeError, Upstream, UpstreamEvent, UpstreamSink, UpstreamStats};

/// Hyperliquid's own per-IP limits. Budgets are capped below these.
pub const HL_MAX_CONNECTIONS: usize = 10;
pub const HL_MAX_SUBSCRIPTIONS: usize = 1000;
pub const HL_MAX_MESSAGES_PER_MIN: u32 = 2000;
pub const HL_MAX_CONNECTS_PER_MIN: u32 = 30;

/// Open another on-demand connection once each one carries this many subscriptions.
const SPREAD_AT: usize = 200;
/// A subscription sent this recently and never acknowledged is blamed for a closed connection.
const REJECT_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub ws_url: String,
    /// Connections, the permanent one included. At least 2, at most 8.
    pub max_connections: usize,
    /// Subscriptions across all connections. At most 950.
    pub max_subscriptions: usize,
    /// Messages sent (subscribe, unsubscribe, ping) per minute across all connections.
    pub messages_per_min: u32,
    pub connects_per_min: u32,
    /// Hyperliquid closes a connection it hasn't sent anything to in 60 s.
    pub ping_every: Duration,
    /// Reconnect when nothing has arrived for this long.
    pub idle_timeout: Duration,
}

impl PoolConfig {
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            max_connections: 3,
            max_subscriptions: 900,
            messages_per_min: 1000,
            connects_per_min: 10,
            ping_every: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(75),
        }
    }

    /// Keep every budget inside Hyperliquid's limits, with room for reconnect overlap (a
    /// dropped socket may still count against the IP for a moment) and other tools on the IP.
    pub fn clamped(mut self) -> Self {
        self.max_connections = self.max_connections.clamp(2, HL_MAX_CONNECTIONS - 2);
        self.max_subscriptions = self.max_subscriptions.clamp(1, HL_MAX_SUBSCRIPTIONS - 50);
        self.messages_per_min = self.messages_per_min.clamp(10, HL_MAX_MESSAGES_PER_MIN - 200);
        self.connects_per_min = self.connects_per_min.clamp(1, HL_MAX_CONNECTS_PER_MIN - 10);
        self
    }
}

/// What every connection task shares. It holds no connection handles, so the tasks end once
/// the pool and every guard are gone.
struct Shared {
    cfg: PoolConfig,
    sends: WindowLimiter,
    connects: WindowLimiter,
    metrics: Arc<Metrics>,
}

struct Conn {
    tx: mpsc::UnboundedSender<Cmd>,
    permanent: bool,
    subs: usize,
    up: Arc<AtomicBool>,
}

struct State {
    conns: Vec<Conn>,
    subs: BTreeMap<SubKey, usize>,
}

struct Inner {
    shared: Arc<Shared>,
    state: Mutex<State>,
}

enum Cmd {
    Subscribe(SubKey, UpstreamSink),
    Unsubscribe(SubKey),
}

#[derive(Clone)]
pub struct Pool(Arc<Inner>);

/// Holds one upstream subscription; dropping it unsubscribes.
pub struct SubGuard {
    pool: Arc<Inner>,
    key: SubKey,
}

impl Drop for SubGuard {
    fn drop(&mut self) {
        let mut st = self.pool.state.lock().expect("pool lock");
        if let Some(i) = st.subs.remove(&self.key) {
            st.conns[i].subs -= 1;
            let _ = st.conns[i].tx.send(Cmd::Unsubscribe(self.key.clone()));
        }
    }
}

impl Pool {
    pub fn new(cfg: PoolConfig, metrics: Arc<Metrics>) -> Self {
        let cfg = cfg.clamped();
        let shared = Arc::new(Shared {
            sends: WindowLimiter::per_minute(cfg.messages_per_min),
            connects: WindowLimiter::per_minute(cfg.connects_per_min),
            cfg,
            metrics,
        });
        Self(Arc::new(Inner { shared, state: Mutex::new(State { conns: Vec::new(), subs: BTreeMap::new() }) }))
    }

    /// Subscribe upstream, within budget. Must run inside a tokio runtime (it may open a
    /// connection).
    pub fn subscribe(&self, key: SubKey, sink: UpstreamSink, permanent: bool) -> Result<SubGuard, SubscribeError> {
        let cfg = &self.0.shared.cfg;
        let mut st = self.0.state.lock().expect("pool lock");
        if st.subs.contains_key(&key) {
            return Err(SubscribeError::Unsupported(format!("{key} is already subscribed")));
        }
        if st.subs.len() >= cfg.max_subscriptions {
            return Err(SubscribeError::Capacity(format!(
                "{} of {} upstream subscriptions in use",
                st.subs.len(),
                cfg.max_subscriptions
            )));
        }
        let least = st
            .conns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.permanent == permanent)
            .min_by_key(|(_, c)| c.subs)
            .map(|(i, c)| (i, c.subs));
        let i = match least {
            Some((i, _)) if permanent => i,
            Some((i, n)) if n < SPREAD_AT || st.conns.len() >= cfg.max_connections => i,
            _ if st.conns.len() < cfg.max_connections => {
                let (tx, rx) = mpsc::unbounded_channel();
                let up = Arc::new(AtomicBool::new(false));
                let id = st.conns.len();
                tokio::spawn(run_conn(self.0.shared.clone(), id, up.clone(), rx));
                st.conns.push(Conn { tx, permanent, subs: 0, up });
                id
            }
            _ => {
                return Err(SubscribeError::Capacity(format!(
                    "all {} upstream connections are in use",
                    cfg.max_connections
                )));
            }
        };
        st.conns[i].subs += 1;
        st.subs.insert(key.clone(), i);
        let _ = st.conns[i].tx.send(Cmd::Subscribe(key.clone(), sink));
        Ok(SubGuard { pool: self.0.clone(), key })
    }

    pub fn stats(&self) -> UpstreamStats {
        let sh = &self.0.shared;
        let st = self.0.state.lock().expect("pool lock");
        let mut by_kind = BTreeMap::new();
        for k in st.subs.keys() {
            *by_kind.entry(k.kind()).or_insert(0) += 1;
        }
        UpstreamStats {
            connections_open: st.conns.len(),
            connections_up: st.conns.iter().filter(|c| c.up.load(Ordering::Relaxed)).count(),
            max_connections: sh.cfg.max_connections,
            subscriptions: st.subs.len(),
            max_subscriptions: sh.cfg.max_subscriptions,
            by_kind,
            per_connection: st.conns.iter().map(|c| c.subs).collect(),
            keys: st.subs.keys().map(ToString::to_string).collect(),
            messages_sent_last_min: sh.sends.used(),
            messages_per_min: sh.sends.budget(),
            connects_last_min: sh.connects.used(),
            ..Default::default()
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

struct Sub {
    sink: UpstreamSink,
    /// Hyperliquid acknowledged it on the current connection.
    acked: bool,
    /// When the subscribe went out on the current connection.
    sent: Option<Instant>,
    /// Sinks were told `Disconnected` and haven't heard `Reconnected` yet.
    stale: bool,
}

impl Sub {
    fn event(&self, event: UpstreamEvent) {
        self.sink.send(Upstream { recv: Instant::now(), recv_ms: now_ms(), event });
    }
}

enum Out {
    Subscribe(SubKey),
    Unsubscribe(SubKey),
    Ping,
}

#[derive(PartialEq)]
enum End {
    /// The connection dropped; reconnect.
    Dropped,
    /// The pool is gone; stop.
    Shutdown,
}

/// Apply a command whether or not the connection is up. Returns false once the pool is gone.
fn apply(cmd: Option<Cmd>, subs: &mut BTreeMap<SubKey, Sub>, out: Option<&mut VecDeque<Out>>) -> bool {
    match cmd {
        None => false,
        Some(Cmd::Subscribe(key, sink)) => {
            subs.insert(key.clone(), Sub { sink, acked: false, sent: None, stale: false });
            if let Some(out) = out {
                out.push_back(Out::Subscribe(key));
            }
            true
        }
        Some(Cmd::Unsubscribe(key)) => {
            if subs.remove(&key).is_some()
                && let Some(out) = out
            {
                out.push_back(Out::Unsubscribe(key));
            }
            true
        }
    }
}

/// Wait out `fut` (a backoff or the connect limiter) while still taking commands.
async fn wait<F: Future<Output = ()>>(
    fut: F,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    subs: &mut BTreeMap<SubKey, Sub>,
) -> bool {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            _ = &mut fut => return true,
            cmd = rx.recv() => if !apply(cmd, subs, None) { return false },
        }
    }
}

async fn run_conn(sh: Arc<Shared>, id: usize, up: Arc<AtomicBool>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut subs: BTreeMap<SubKey, Sub> = BTreeMap::new();
    let mut backoff = Duration::from_secs(1);
    let mut connected_before = false;
    loop {
        if !wait(sh.connects.acquire(1), &mut rx, &mut subs).await {
            return;
        }
        match tokio_tungstenite::connect_async(sh.cfg.ws_url.as_str()).await {
            Ok((ws, _)) => {
                up.store(true, Ordering::Relaxed);
                if connected_before {
                    Metrics::add(&sh.metrics.upstream_reconnects, 1);
                }
                connected_before = true;
                tracing::info!("upstream connection {id} up ({} subscriptions)", subs.len());
                let started = Instant::now();
                let end = session(&sh, ws, &mut subs, &mut rx).await;
                up.store(false, Ordering::Relaxed);
                blame_unacked(&sh, &mut subs);
                for s in subs.values_mut().filter(|s| !s.stale) {
                    s.event(UpstreamEvent::Disconnected);
                    s.stale = true;
                }
                if end == End::Shutdown {
                    return;
                }
                tracing::warn!("upstream connection {id} dropped after {:?}", started.elapsed());
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(e) => tracing::warn!("upstream connection {id}: {e}"),
        }
        // Up to 25% jitter so connections don't reconnect in lockstep.
        let jitter = backoff.mul_f64(
            f64::from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() % 250) / 1000.0,
        );
        if !wait(tokio::time::sleep(backoff + jitter), &mut rx, &mut subs).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Hyperliquid answers a subscription to a coin it doesn't list by closing the connection,
/// with no error message. A subscription sent just before the close and never acknowledged
/// is the likely cause: drop it rather than resubscribe it into a reconnect loop.
fn blame_unacked(sh: &Shared, subs: &mut BTreeMap<SubKey, Sub>) {
    let suspects: Vec<SubKey> = subs
        .iter()
        .filter(|(_, s)| !s.acked && s.sent.is_some_and(|t| t.elapsed() < REJECT_WINDOW))
        .map(|(k, _)| k.clone())
        .collect();
    for key in suspects {
        tracing::warn!("upstream closed the connection right after `{key}`; not resubscribing it");
        Metrics::add(&sh.metrics.upstream_rejected, 1);
        if let Some(s) = subs.remove(&key) {
            s.event(UpstreamEvent::Rejected(format!("the upstream refused {key}")));
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn session(sh: &Shared, ws: Ws, subs: &mut BTreeMap<SubKey, Sub>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> End {
    let (mut tx, mut stream) = ws.split();
    let mut out: VecDeque<Out> = subs.keys().cloned().map(Out::Subscribe).collect();
    for s in subs.values_mut() {
        s.acked = false;
        s.sent = None;
    }
    // Keys whose subscribe went out on this connection, so an unsubscribe is only ever sent
    // for something actually subscribed.
    let mut active: HashSet<SubKey> = HashSet::new();
    let mut ping = tokio::time::interval(sh.cfg.ping_every);
    ping.tick().await;
    let mut last_rx = Instant::now();
    loop {
        // Drop queued messages that no longer apply before spending send budget on them.
        while let Some(o) = out.front() {
            let stale = match o {
                Out::Subscribe(k) => !subs.contains_key(k) || active.contains(k),
                Out::Unsubscribe(k) => !active.contains(k) || subs.contains_key(k),
                Out::Ping => false,
            };
            if !stale {
                break;
            }
            out.pop_front();
        }
        tokio::select! {
            biased;
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let recv = Instant::now();
                    last_rx = recv;
                    Metrics::add(&sh.metrics.upstream_messages, 1);
                    Metrics::add(&sh.metrics.upstream_bytes, text.len() as u64);
                    handle(text.as_str(), recv, subs);
                }
                Some(Ok(Message::Close(_))) | None => return End::Dropped,
                Some(Ok(_)) => last_rx = Instant::now(),
                Some(Err(e)) => {
                    tracing::debug!("upstream read: {e}");
                    return End::Dropped;
                }
            },
            cmd = rx.recv() => if !apply(cmd, subs, Some(&mut out)) {
                let _ = tx.close().await;
                return End::Shutdown;
            },
            _ = sh.sends.acquire(1), if !out.is_empty() => {
                let text = match out.pop_front().expect("checked non-empty") {
                    Out::Subscribe(k) => {
                        if let Some(s) = subs.get_mut(&k) {
                            s.sent = Some(Instant::now());
                        }
                        let m = k.message("subscribe");
                        active.insert(k);
                        m
                    }
                    Out::Unsubscribe(k) => {
                        active.remove(&k);
                        k.message("unsubscribe")
                    }
                    Out::Ping => wire::PING.to_string(),
                };
                if let Err(e) = tx.send(Message::Text(text.into())).await {
                    tracing::debug!("upstream write: {e}");
                    return End::Dropped;
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > sh.cfg.idle_timeout {
                    tracing::warn!("upstream silent for {:?}; reconnecting", last_rx.elapsed());
                    return End::Dropped;
                }
                out.push_back(Out::Ping);
            }
        }
    }
}

fn handle(text: &str, recv: Instant, subs: &mut BTreeMap<SubKey, Sub>) {
    let recv_ms = now_ms();
    match wire::parse(text) {
        Ok(Incoming::Data(key, event)) => {
            if let Some(s) = subs.get_mut(&key) {
                if s.stale {
                    s.stale = false;
                    s.sink.send(Upstream { recv, recv_ms, event: UpstreamEvent::Reconnected });
                }
                s.acked = true;
                s.sink.send(Upstream { recv, recv_ms, event });
            }
        }
        Ok(Incoming::Subscribed(key)) => {
            if let Some(s) = subs.get_mut(&key) {
                s.acked = true;
                if s.stale {
                    s.stale = false;
                    s.sink.send(Upstream { recv, recv_ms, event: UpstreamEvent::Reconnected });
                }
            }
        }
        Ok(Incoming::Error(e)) => tracing::warn!("upstream error: {e}"),
        Ok(Incoming::Unsubscribed(_) | Incoming::Pong | Incoming::Ignored) => {}
        Err(e) => tracing::debug!("upstream message skipped: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> UpstreamSink {
        UpstreamSink::new(|_| {})
    }

    fn book(c: &str) -> SubKey {
        SubKey::L2Book { coin: c.into() }
    }

    fn pool(max_connections: usize, max_subscriptions: usize) -> Pool {
        // Nothing listens here: the connection tasks retry in the background, which these
        // tests don't need. They only check the pool's accounting.
        let mut cfg = PoolConfig::new("ws://localhost:9/ws");
        cfg.max_connections = max_connections;
        cfg.max_subscriptions = max_subscriptions;
        Pool::new(cfg, Arc::new(Metrics::default()))
    }

    #[test]
    fn budgets_are_capped_below_hyperliquid_limits() {
        let mut cfg = PoolConfig::new("x");
        cfg.max_connections = 50;
        cfg.max_subscriptions = 5000;
        cfg.messages_per_min = 9999;
        cfg.connects_per_min = 100;
        let c = cfg.clamped();
        assert!(c.max_connections < HL_MAX_CONNECTIONS && c.max_subscriptions < HL_MAX_SUBSCRIPTIONS);
        assert!(c.messages_per_min < HL_MAX_MESSAGES_PER_MIN && c.connects_per_min < HL_MAX_CONNECTS_PER_MIN);
        assert_eq!(PoolConfig { max_connections: 1, ..PoolConfig::new("x") }.clamped().max_connections, 2);
    }

    #[tokio::test]
    async fn refuses_subscriptions_past_the_budget() {
        let p = pool(3, 4);
        let mids = p.subscribe(SubKey::AllMids { dex: String::new() }, sink(), true).unwrap();
        let a = p.subscribe(book("BTC"), sink(), false).unwrap();
        let _b = p.subscribe(book("ETH"), sink(), false).unwrap();
        let _c = p.subscribe(SubKey::Trades { coin: "BTC".into() }, sink(), false).unwrap();
        assert!(matches!(p.subscribe(book("SOL"), sink(), false), Err(SubscribeError::Capacity(_))));
        assert!(matches!(p.subscribe(book("ETH"), sink(), false), Err(SubscribeError::Unsupported(_))));
        let s = p.stats();
        assert_eq!((s.subscriptions, s.connections_open), (4, 2), "mids alone on one, the rest on another");
        assert_eq!(s.per_connection, vec![1, 3]);
        assert_eq!(s.by_kind.get("l2Book"), Some(&2));
        // Releasing one frees a slot.
        drop(a);
        assert_eq!(p.stats().subscriptions, 3);
        let _d = p.subscribe(book("SOL"), sink(), false).unwrap();
        drop(mids);
        assert_eq!(p.stats().per_connection, vec![0, 3]);
    }

    #[tokio::test]
    async fn never_opens_more_connections_than_the_budget() {
        let p = pool(2, 900);
        let _m = p.subscribe(SubKey::AllMids { dex: "xyz".into() }, sink(), true).unwrap();
        let guards: Vec<_> =
            (0..SPREAD_AT + 20).map(|i| p.subscribe(book(&format!("C{i}")), sink(), false).unwrap()).collect();
        let s = p.stats();
        assert_eq!(s.connections_open, 2);
        assert_eq!(s.per_connection, vec![1, SPREAD_AT + 20], "past SPREAD_AT the last connection keeps filling");
        drop(guards);
        let p = pool(3, 900);
        let _m = p.subscribe(SubKey::AllMids { dex: String::new() }, sink(), true).unwrap();
        let _g: Vec<_> =
            (0..SPREAD_AT + 1).map(|i| p.subscribe(book(&format!("C{i}")), sink(), false).unwrap()).collect();
        assert_eq!(p.stats().per_connection, vec![1, SPREAD_AT, 1], "a third connection opens once the second is full");
    }
}
