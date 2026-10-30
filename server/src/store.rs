//! The hot store (`docs/architecture/server.md` §3.3): the latest state of every topic in memory,
//! fanned out to client sessions.
//!
//! - Mids are permanent: one upstream subscription per venue, merged into one table.
//! - Book, trades and bars topics exist only while some client watches them. Each holds a
//!   refcount; when it drops to zero the topic lingers briefly, then its upstream subscription
//!   is dropped.
//! - The book is a state topic (`watch`: the latest value wins, so slow readers skip ahead).
//!   Trades and bars are event topics: a bounded `broadcast` ring with a per-topic sequence
//!   number, so a reader that falls behind the ring knows it did and resyncs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, watch};
use tr_core::{AssetClass, Bar, Security, Timeframe};
use tr_proto::{BarDelta, InstrumentKind, Level, MarketInfo, MarketKey, TopicStatus, Trade, UnavailableReason};

use crate::metrics::Metrics;
use crate::provider::{
    Channel, InstrumentDef, InstrumentId, Provider, SubscribeError, Upstream, UpstreamEvent, UpstreamHandle,
    UpstreamSink,
};

#[derive(Debug, Clone)]
pub struct HubConfig {
    /// How long a topic nobody watches keeps its upstream subscription.
    pub linger: Duration,
    /// Recent trades kept for snapshots.
    pub trades_kept: usize,
    /// Closed bars kept to overlay on fetched history.
    pub bars_kept: usize,
    /// Event ring size per topic. A reader further behind than this resyncs.
    pub ring: usize,
    /// How long fetched history is reused.
    pub history_ttl: Duration,
    /// How often the instrument list (and previous-day prices) is refreshed.
    pub registry_every: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            linger: Duration::from_secs(20),
            trades_kept: 100,
            bars_kept: 64,
            ring: 1024,
            history_ttl: Duration::from_secs(15),
            registry_every: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mid {
    pub px: f64,
    pub ts_ms: i64,
    pub recv: Instant,
    /// The table version that last changed it.
    pub stamp: u64,
    /// The venue whose feed carries it (`""` is the core dex, which also carries spot).
    pub venue: Arc<str>,
}

/// One venue's mids feed.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueState {
    pub status: TopicStatus,
    /// Bumped each time the feed reconnects, so readers know to resync.
    pub epoch: u64,
}

impl Default for VenueState {
    fn default() -> Self {
        Self { status: TopicStatus::Live, epoch: 0 }
    }
}

/// Every mid on every venue, each venue's feed state, and a version readers watch for changes
/// to either.
pub struct MidsTable {
    map: RwLock<HashMap<String, Mid>>,
    venues: RwLock<HashMap<String, VenueState>>,
    version: watch::Sender<u64>,
}

impl Default for MidsTable {
    fn default() -> Self {
        Self { map: RwLock::default(), venues: RwLock::default(), version: watch::channel(0).0 }
    }
}

impl MidsTable {
    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    /// One message from a venue's feed. Hyperliquid sends every mid the venue has each time, so
    /// a coin missing from it is gone (delisted) and leaves the table.
    fn apply(&self, venue: &str, recv: Instant, ts_ms: i64, mids: Vec<(String, f64)>) {
        let v = *self.version.borrow() + 1;
        let mut changed = false;
        {
            let mut map = self.map.write().expect("mids lock");
            let present: HashSet<&str> = mids.iter().map(|(c, _)| c.as_str()).collect();
            let before = map.len();
            map.retain(|c, m| &*m.venue != venue || present.contains(c.as_str()));
            changed |= map.len() != before;
            let name: Arc<str> = Arc::from(venue);
            for (coin, px) in mids {
                match map.get_mut(&coin) {
                    Some(m) if m.px == px => {}
                    Some(m) => {
                        *m = Mid { px, ts_ms, recv, stamp: v, venue: name.clone() };
                        changed = true;
                    }
                    None => {
                        map.insert(coin, Mid { px, ts_ms, recv, stamp: v, venue: name.clone() });
                        changed = true;
                    }
                }
            }
        }
        changed |= self.set_venue(venue, |st| st.status = TopicStatus::Live);
        if changed {
            self.version.send_replace(v);
        }
    }

    /// Change a venue's state; true if it changed. Doesn't bump the version.
    fn set_venue(&self, venue: &str, f: impl FnOnce(&mut VenueState)) -> bool {
        let mut venues = self.venues.write().expect("venues lock");
        let st = venues.entry(venue.to_string()).or_default();
        let before = st.clone();
        f(st);
        *st != before
    }

    /// Connection news for a venue's feed.
    fn venue_event(&self, venue: &str, recv_ms: i64, ev: &UpstreamEvent) {
        let changed = match ev {
            UpstreamEvent::Disconnected => {
                self.set_venue(venue, |st| st.status = TopicStatus::Stale { since_ms: recv_ms })
            }
            UpstreamEvent::Reconnected => self.set_venue(venue, |st| st.epoch += 1),
            UpstreamEvent::Rejected(why) => self.set_venue(venue, |st| {
                st.status = TopicStatus::Unavailable { reason: UnavailableReason::Upstream, detail: why.clone() }
            }),
            _ => false,
        };
        if changed {
            self.bump();
        }
    }

    pub fn venue(&self, venue: &str) -> VenueState {
        self.venues.read().expect("venues lock").get(venue).cloned().unwrap_or_default()
    }

    pub fn get(&self, coin: &str) -> Option<Mid> {
        self.map.read().expect("mids lock").get(coin).cloned()
    }

    pub fn contains(&self, coin: &str) -> bool {
        self.map.read().expect("mids lock").contains_key(coin)
    }

    pub fn len(&self) -> usize {
        self.map.read().expect("mids lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn coins(&self) -> Vec<String> {
        self.map.read().expect("mids lock").keys().cloned().collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TopicKey {
    Book(InstrumentId),
    Trades(InstrumentId),
    Bars(InstrumentId, Timeframe),
}

impl TopicKey {
    fn channel(&self) -> (Channel, &InstrumentId) {
        match self {
            TopicKey::Book(i) => (Channel::Book, i),
            TopicKey::Trades(i) => (Channel::Trades, i),
            TopicKey::Bars(i, tf) => (Channel::Bars(*tf), i),
        }
    }
}

#[derive(Clone)]
pub enum Topic {
    Book(Arc<BookTopic>),
    Trades(Arc<TradesTopic>),
    Bars(Arc<BarsTopic>),
}

impl Topic {
    fn new(key: &TopicKey, cfg: &HubConfig) -> Self {
        match key {
            TopicKey::Book(i) => Topic::Book(Arc::new(BookTopic::new(i.clone()))),
            TopicKey::Trades(i) => Topic::Trades(Arc::new(TradesTopic::new(i.clone(), cfg))),
            TopicKey::Bars(i, tf) => Topic::Bars(Arc::new(BarsTopic::new(i.clone(), *tf, cfg))),
        }
    }

    fn sink(&self) -> UpstreamSink {
        match self.clone() {
            Topic::Book(t) => UpstreamSink::new(move |up| t.on_upstream(up)),
            Topic::Trades(t) => UpstreamSink::new(move |up| t.on_upstream(up)),
            Topic::Bars(t) => UpstreamSink::new(move |up| t.on_upstream(up)),
        }
    }

    fn fail(&self, status: TopicStatus) {
        match self {
            Topic::Book(t) => t.state.send_modify(|s| s.status = status),
            Topic::Trades(t) => t.set_status(Instant::now(), status),
            Topic::Bars(t) => t.set_status(Instant::now(), status),
        }
    }
}

// ---- book (state topic) ----

pub struct BookSnap {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub time_ms: i64,
    pub recv: Instant,
}

#[derive(Clone)]
pub struct BookState {
    pub book: Option<Arc<BookSnap>>,
    /// Bumped when the upstream reconnects: readers resync.
    pub epoch: u64,
    pub status: TopicStatus,
}

pub struct BookTopic {
    pub coin: InstrumentId,
    pub state: watch::Sender<BookState>,
}

impl BookTopic {
    fn new(coin: InstrumentId) -> Self {
        Self { coin, state: watch::channel(BookState { book: None, epoch: 0, status: TopicStatus::Live }).0 }
    }

    fn on_upstream(&self, up: Upstream) {
        match up.event {
            UpstreamEvent::Book { bids, asks, time_ms } => self.state.send_modify(|s| {
                s.book = Some(Arc::new(BookSnap { bids, asks, time_ms, recv: up.recv }));
                s.status = TopicStatus::Live;
            }),
            UpstreamEvent::Disconnected => {
                self.state.send_modify(|s| s.status = TopicStatus::Stale { since_ms: up.recv_ms })
            }
            UpstreamEvent::Reconnected => self.state.send_modify(|s| {
                s.epoch += 1;
                s.book = None;
            }),
            UpstreamEvent::Rejected(why) => self.state.send_modify(|s| {
                s.status = TopicStatus::Unavailable { reason: UnavailableReason::Upstream, detail: why };
            }),
            _ => {}
        }
    }
}

// ---- event topics ----

pub struct Event<T> {
    pub seq: u64,
    pub recv: Instant,
    pub kind: EventKind<T>,
}

pub enum EventKind<T> {
    Data(T),
    /// The upstream reconnected and may have skipped data: take a fresh snapshot.
    Resync,
    Status(TopicStatus),
}

struct Ring<S, T> {
    state: S,
    seq: u64,
    status: TopicStatus,
    /// Set by `Reconnected`; the next data is announced as a resync instead.
    resync_pending: bool,
    tx: broadcast::Sender<Arc<Event<T>>>,
}

impl<S, T> Ring<S, T> {
    fn new(state: S, ring: usize) -> Self {
        Self { state, seq: 0, status: TopicStatus::Live, resync_pending: false, tx: broadcast::channel(ring.max(2)).0 }
    }

    fn emit(&mut self, recv: Instant, kind: EventKind<T>) {
        self.seq += 1;
        let _ = self.tx.send(Arc::new(Event { seq: self.seq, recv, kind }));
    }

    fn set_status(&mut self, recv: Instant, status: TopicStatus) {
        if self.status != status {
            self.status = status.clone();
            self.emit(recv, EventKind::Status(status));
        }
    }
}

pub struct TradesTopic {
    pub coin: InstrumentId,
    kept: usize,
    ring: Mutex<Ring<VecDeque<Trade>, Vec<Trade>>>,
}

impl TradesTopic {
    fn new(coin: InstrumentId, cfg: &HubConfig) -> Self {
        Self { coin, kept: cfg.trades_kept, ring: Mutex::new(Ring::new(VecDeque::new(), cfg.ring)) }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event<Vec<Trade>>>> {
        self.ring.lock().expect("trades lock").tx.subscribe()
    }

    /// Recent trades (oldest first), the sequence number they are current to, and the status.
    pub fn snapshot(&self) -> (Vec<Trade>, u64, TopicStatus) {
        let r = self.ring.lock().expect("trades lock");
        (r.state.iter().copied().collect(), r.seq, r.status.clone())
    }

    fn set_status(&self, recv: Instant, status: TopicStatus) {
        self.ring.lock().expect("trades lock").set_status(recv, status);
    }

    fn on_upstream(&self, up: Upstream) {
        let mut r = self.ring.lock().expect("trades lock");
        match up.event {
            UpstreamEvent::Trades(batch) => {
                // A (re)subscription starts with recent history that may overlap what we hold.
                let mut fresh: Vec<Trade> = batch
                    .into_iter()
                    .filter(|t| !r.state.iter().any(|k| k.tid == t.tid && k.time_ms == t.time_ms))
                    .collect();
                fresh.sort_by_key(|t| (t.time_ms, t.tid));
                r.state.extend(fresh.iter().copied());
                while r.state.len() > self.kept {
                    r.state.pop_front();
                }
                r.set_status(up.recv, TopicStatus::Live);
                if std::mem::take(&mut r.resync_pending) {
                    r.emit(up.recv, EventKind::Resync);
                } else if !fresh.is_empty() {
                    r.emit(up.recv, EventKind::Data(fresh));
                }
            }
            UpstreamEvent::Disconnected => r.set_status(up.recv, TopicStatus::Stale { since_ms: up.recv_ms }),
            UpstreamEvent::Reconnected => r.resync_pending = true,
            UpstreamEvent::Rejected(why) => {
                r.set_status(up.recv, TopicStatus::Unavailable { reason: UnavailableReason::Upstream, detail: why })
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct BarsState {
    closed: VecDeque<Bar>,
    forming: Option<Bar>,
    /// When the stream that `closed` and `forming` come from started (first bar after
    /// subscribing or resyncing). History fetched after this overlaps it with no gap.
    live_since: Option<Instant>,
}

pub struct BarsTopic {
    pub coin: InstrumentId,
    pub tf: Timeframe,
    kept: usize,
    ring: Mutex<Ring<BarsState, BarDelta>>,
}

/// What the live stream has seen, to lay over fetched history.
pub struct Overlay {
    /// Closed bars, then the forming bar.
    pub bars: Vec<Bar>,
    pub seq: u64,
    pub live_since: Option<Instant>,
    pub status: TopicStatus,
}

impl BarsTopic {
    fn new(coin: InstrumentId, tf: Timeframe, cfg: &HubConfig) -> Self {
        Self { coin, tf, kept: cfg.bars_kept, ring: Mutex::new(Ring::new(BarsState::default(), cfg.ring)) }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event<BarDelta>>> {
        self.ring.lock().expect("bars lock").tx.subscribe()
    }

    pub fn overlay(&self) -> Overlay {
        let r = self.ring.lock().expect("bars lock");
        let mut bars: Vec<Bar> = r.state.closed.iter().copied().collect();
        bars.extend(r.state.forming);
        Overlay { bars, seq: r.seq, live_since: r.state.live_since, status: r.status.clone() }
    }

    fn set_status(&self, recv: Instant, status: TopicStatus) {
        self.ring.lock().expect("bars lock").set_status(recv, status);
    }

    fn on_upstream(&self, up: Upstream) {
        let mut r = self.ring.lock().expect("bars lock");
        match up.event {
            UpstreamEvent::Bar(b) => {
                r.set_status(up.recv, TopicStatus::Live);
                if std::mem::take(&mut r.resync_pending) {
                    // A bar may have closed while we were away; start over from fresh history.
                    r.state = BarsState { closed: VecDeque::new(), forming: Some(b), live_since: Some(up.recv) };
                    r.emit(up.recv, EventKind::Resync);
                    return;
                }
                r.state.live_since.get_or_insert(up.recv);
                match r.state.forming {
                    Some(f) if b.ts < f.ts => {}
                    Some(f) if b.ts == f.ts => {
                        if f != b {
                            r.state.forming = Some(b);
                            r.emit(up.recv, EventKind::Data(BarDelta::Forming(b)));
                        }
                    }
                    prev => {
                        // A new bar opened: the previous one's last state is final. It goes out
                        // before the new bar, and it is never conflated.
                        if let Some(f) = prev {
                            r.state.closed.push_back(f);
                            while r.state.closed.len() > self.kept {
                                r.state.closed.pop_front();
                            }
                            r.emit(up.recv, EventKind::Data(BarDelta::Closed(f)));
                        }
                        r.state.forming = Some(b);
                        r.emit(up.recv, EventKind::Data(BarDelta::Forming(b)));
                    }
                }
            }
            UpstreamEvent::Disconnected => r.set_status(up.recv, TopicStatus::Stale { since_ms: up.recv_ms }),
            UpstreamEvent::Reconnected => r.resync_pending = true,
            UpstreamEvent::Rejected(why) => {
                r.set_status(up.recv, TopicStatus::Unavailable { reason: UnavailableReason::Upstream, detail: why })
            }
            _ => {}
        }
    }
}

/// Lay live bars over history: same `ts` replaces, newer appends. Keeps the last `limit`.
pub fn merge_bars(mut history: Vec<Bar>, live: &[Bar], limit: usize) -> Vec<Bar> {
    for b in live {
        match history.binary_search_by_key(&b.ts, |h| h.ts) {
            Ok(i) => history[i] = *b,
            Err(i) if i == history.len() => history.push(*b),
            Err(i) => history.insert(i, *b),
        }
    }
    if history.len() > limit {
        history.drain(..history.len() - limit);
    }
    history
}

// ---- the hub ----

struct Entry {
    /// Unique per entry, so a reference to a failed and replaced entry can't release the new one.
    id: u64,
    refs: usize,
    /// Bumped on every acquire, so a pending linger knows it was superseded.
    generation: u64,
    topic: Topic,
    upstream: Option<UpstreamHandle>,
}

struct Cached {
    at: Instant,
    limit: usize,
    bars: Arc<Vec<Bar>>,
}

pub struct Hub {
    provider: Arc<dyn Provider>,
    pub cfg: HubConfig,
    pub metrics: Arc<Metrics>,
    pub mids: MidsTable,
    registry: RwLock<HashMap<String, InstrumentDef>>,
    venues: Mutex<HashMap<String, UpstreamHandle>>,
    topics: Mutex<HashMap<TopicKey, Entry>>,
    next_entry: std::sync::atomic::AtomicU64,
    /// The registry loop, until [`Hub::stop`].
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stopped: std::sync::atomic::AtomicBool,
    history: Mutex<HashMap<(InstrumentId, Timeframe), Cached>>,
}

/// A client's hold on a topic. Dropping it releases the topic.
pub struct TopicRef {
    hub: Arc<Hub>,
    key: TopicKey,
    entry: u64,
    pub topic: Topic,
}

impl Drop for TopicRef {
    fn drop(&mut self) {
        self.hub.release(&self.key, self.entry);
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TopicCounts {
    pub book: usize,
    pub trades: usize,
    pub bars: usize,
    /// Topics nobody watches that are waiting out their linger.
    pub lingering: usize,
    pub mids: usize,
}

pub type Unavailable = (UnavailableReason, String);

impl Hub {
    pub fn new(provider: Arc<dyn Provider>, cfg: HubConfig, metrics: Arc<Metrics>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            cfg,
            metrics,
            mids: MidsTable::default(),
            registry: RwLock::default(),
            venues: Mutex::default(),
            topics: Mutex::default(),
            next_entry: Default::default(),
            task: Mutex::default(),
            stopped: Default::default(),
            history: Mutex::default(),
        })
    }

    pub fn provider(&self) -> &dyn Provider {
        self.provider.as_ref()
    }

    /// Load the instrument list, subscribe every venue's mids, and keep both fresh.
    pub fn start(self: &Arc<Self>) {
        let hub = self.clone();
        let task = tokio::spawn(async move {
            loop {
                let wait = match hub.refresh_registry().await {
                    Ok(n) => {
                        tracing::info!(
                            "{n} instruments listed; mids from {} venues",
                            hub.venues.lock().expect("venues lock").len()
                        );
                        hub.cfg.registry_every
                    }
                    Err(e) => {
                        tracing::warn!("instrument list: {e:#}; retrying");
                        Duration::from_secs(10)
                    }
                };
                tokio::time::sleep(wait).await;
            }
        });
        *self.task.lock().expect("task lock") = Some(task);
    }

    /// Stop for good: end the registry loop, drop every upstream subscription and close the
    /// provider's connections. Clients still connected get no more data.
    pub fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.task.lock().expect("task lock").take() {
            t.abort();
        }
        let venues = std::mem::take(&mut *self.venues.lock().expect("venues lock"));
        let topics = std::mem::take(&mut *self.topics.lock().expect("topics lock"));
        // Dropping the handles unsubscribes upstream; do it outside the hub's locks.
        drop((venues, topics));
        self.provider.close();
    }

    async fn refresh_registry(self: &Arc<Self>) -> Result<usize> {
        let list = self.provider.instruments().await?;
        let n = list.len();
        let mut venues: Vec<String> =
            list.iter().filter(|d| d.kind == InstrumentKind::Perp).map(|d| d.venue.clone()).collect();
        venues.sort();
        venues.dedup();
        *self.registry.write().expect("registry lock") = list.into_iter().map(|d| (d.id.0.clone(), d)).collect();
        for venue in venues {
            if self.venues.lock().expect("venues lock").contains_key(&venue) {
                continue;
            }
            let hub = Arc::downgrade(self);
            let name = venue.clone();
            let sink = UpstreamSink::new(move |up| {
                let Some(hub) = hub.upgrade() else { return };
                match up.event {
                    UpstreamEvent::Mids(m) => hub.mids.apply(&name, up.recv, up.recv_ms, m),
                    ev => hub.mids.venue_event(&name, up.recv_ms, &ev),
                }
            });
            match self.provider.subscribe(Channel::Mids, &InstrumentId(venue.clone()), sink).await {
                Ok(h) => {
                    self.venues.lock().expect("venues lock").insert(venue, h);
                }
                Err(e) => tracing::warn!("mids for venue {venue:?}: {e}"),
            }
        }
        Ok(n)
    }

    /// Wait (up to `timeout`) for the first mids, which tell us what exists.
    pub async fn ready(&self, timeout: Duration) -> bool {
        let mut rx = self.mids.subscribe();
        let wait = async {
            while self.mids.is_empty() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(timeout, wait).await;
        !self.mids.is_empty()
    }

    /// Map a client's market key to the upstream instrument. Venue-native keys (`BTC`,
    /// `xyz:TSLA`, `@107`, `PURR/USDC`) pass through when the venue lists them. Crypto market
    /// keys (`BTCUSD Curncy`) map to the core perp, which every quote labels with its source.
    /// Nothing maps an equity key to a HIP-3 perp yet (open question 9 in the design).
    pub fn resolve(&self, key: &MarketKey) -> Option<InstrumentId> {
        let k = key.as_str().trim();
        let known = |c: &str| self.mids.contains(c).then(|| InstrumentId::new(c));
        if let Some(i) = known(k) {
            return Some(i);
        }
        if let Some(base) = k.strip_suffix("-PERP")
            && let Some(i) = known(base)
        {
            return Some(i);
        }
        let sec = Security::parse(k)?;
        let (base, quote) = sec.pair()?;
        (sec.class == AssetClass::Crypto && matches!(quote, "USD" | "USDT" | "USDC")).then_some(())?;
        known(base)
    }

    pub fn prev_day(&self, coin: &str) -> Option<f64> {
        self.registry.read().expect("registry lock").get(coin).and_then(|d| d.prev_day)
    }

    /// Every market with a live mid.
    pub fn markets(&self) -> Vec<MarketInfo> {
        let reg = self.registry.read().expect("registry lock");
        let mut out: Vec<MarketInfo> = self
            .mids
            .coins()
            .into_iter()
            .map(|c| {
                let (venue, kind) = match reg.get(&c) {
                    Some(d) => (d.venue.clone(), d.kind),
                    None if c.starts_with('@') || c.contains('/') => ("spot".into(), InstrumentKind::Spot),
                    None => (String::new(), InstrumentKind::Perp),
                };
                MarketInfo { key: MarketKey(c), venue, kind }
            })
            .collect();
        out.sort_by(|a, b| (&a.venue, &a.key).cmp(&(&b.venue, &b.key)));
        out
    }

    /// Hold a topic, creating it and its upstream subscription if nobody holds it yet.
    pub async fn acquire(self: &Arc<Self>, key: TopicKey) -> Result<TopicRef, Unavailable> {
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return Err((UnavailableReason::Upstream, "the server is shutting down".into()));
        }
        let (entry, topic, created) = {
            let mut topics = self.topics.lock().expect("topics lock");
            match topics.get_mut(&key) {
                Some(e) => {
                    e.refs += 1;
                    e.generation += 1;
                    (e.id, e.topic.clone(), false)
                }
                None => {
                    let id = self.next_entry.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let topic = Topic::new(&key, &self.cfg);
                    topics.insert(
                        key.clone(),
                        Entry { id, refs: 1, generation: 0, topic: topic.clone(), upstream: None },
                    );
                    (id, topic, true)
                }
            }
        };
        // Whoever joins while the creator subscribes shares the topic, and its status if that fails.
        let held = TopicRef { hub: self.clone(), key, entry, topic };
        if !created {
            return Ok(held);
        }
        let (ch, inst) = held.key.channel();
        match self.provider.subscribe(ch, inst, held.topic.sink()).await {
            Ok(h) => {
                if let Some(e) = self.topics.lock().expect("topics lock").get_mut(&held.key).filter(|e| e.id == entry) {
                    e.upstream = Some(h);
                }
                Ok(held)
            }
            Err(e) => {
                let reason = match e {
                    SubscribeError::Capacity(_) => UnavailableReason::Capacity,
                    SubscribeError::UnknownInstrument(_) => UnavailableReason::UnknownMarket,
                    SubscribeError::Unsupported(_) => UnavailableReason::Unsupported,
                };
                held.topic.fail(TopicStatus::Unavailable { reason, detail: e.to_string() });
                // The next acquire starts over with a fresh subscription attempt.
                self.topics.lock().expect("topics lock").remove(&held.key);
                Err((reason, e.to_string()))
            }
        }
    }

    fn release(self: &Arc<Self>, key: &TopicKey, entry: u64) {
        let mut topics = self.topics.lock().expect("topics lock");
        let Some(e) = topics.get_mut(key).filter(|e| e.id == entry) else { return };
        e.refs -= 1;
        if e.refs > 0 {
            return;
        }
        let generation = e.generation;
        if self.cfg.linger.is_zero() || tokio::runtime::Handle::try_current().is_err() {
            topics.remove(key);
            return;
        }
        let hub = self.clone();
        let key = key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(hub.cfg.linger).await;
            let mut topics = hub.topics.lock().expect("topics lock");
            if topics.get(&key).is_some_and(|e| e.id == entry && e.refs == 0 && e.generation == generation) {
                // Dropping the entry drops its upstream handle, which unsubscribes upstream.
                topics.remove(&key);
            }
        });
    }

    /// The live bars topic for `(inst, tf)`, if a client holds one.
    pub fn bars_topic(&self, inst: &InstrumentId, tf: Timeframe) -> Option<Arc<BarsTopic>> {
        match self.topics.lock().expect("topics lock").get(&TopicKey::Bars(inst.clone(), tf)) {
            Some(Entry { topic: Topic::Bars(t), .. }) => Some(t.clone()),
            _ => None,
        }
    }

    /// History with the same contract as `Provider::history`. Recent history (`end` = `None`)
    /// is reused for a few seconds, but only if it was fetched after `fresh_after`.
    pub async fn history(
        &self,
        inst: &InstrumentId,
        tf: Timeframe,
        end: Option<i64>,
        limit: usize,
        fresh_after: Option<Instant>,
    ) -> Result<Arc<Vec<Bar>>> {
        let ckey = (inst.clone(), tf);
        if end.is_none()
            && let Some(c) = self.history.lock().expect("history lock").get(&ckey)
            && c.at.elapsed() < self.cfg.history_ttl
            && fresh_after.is_none_or(|t| c.at >= t)
            && (c.limit >= limit || c.bars.len() < c.limit)
        {
            let bars = &c.bars[c.bars.len().saturating_sub(limit)..];
            return Ok(Arc::new(bars.to_vec()));
        }
        let at = Instant::now();
        let bars = Arc::new(self.provider.history(inst, tf, end, limit).await?);
        if end.is_none() {
            self.history.lock().expect("history lock").insert(ckey, Cached { at, limit, bars: bars.clone() });
        }
        Ok(bars)
    }

    pub fn topic_counts(&self) -> TopicCounts {
        let topics = self.topics.lock().expect("topics lock");
        let mut c = TopicCounts { mids: self.mids.len(), ..Default::default() };
        for e in topics.values() {
            match e.topic {
                Topic::Book(_) => c.book += 1,
                Topic::Trades(_) => c.trades += 1,
                Topic::Bars(_) => c.bars += 1,
            }
            if e.refs == 0 {
                c.lingering += 1;
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(event: UpstreamEvent) -> Upstream {
        Upstream { recv: Instant::now(), recv_ms: 1, event }
    }

    fn bar(ts: i64, c: f64) -> Bar {
        Bar::new(ts, 1.0, c.max(1.0), 0.5, c, ts as f64)
    }

    #[test]
    fn bars_close_before_the_next_one_forms() {
        let t = BarsTopic::new(InstrumentId::new("BTC"), Timeframe::M1, &HubConfig::default());
        let mut rx = t.subscribe();
        for b in [bar(0, 1.0), bar(0, 2.0), bar(0, 2.0), bar(60, 3.0), bar(0, 9.0), bar(60, 4.0)] {
            t.on_upstream(up(UpstreamEvent::Bar(b)));
        }
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let EventKind::Data(d) = &e.kind {
                got.push((e.seq, *d));
            }
        }
        let want = [
            BarDelta::Forming(bar(0, 1.0)),
            BarDelta::Forming(bar(0, 2.0)),
            BarDelta::Closed(bar(0, 2.0)),
            BarDelta::Forming(bar(60, 3.0)),
            BarDelta::Forming(bar(60, 4.0)),
        ];
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), want, "duplicates and older bars are skipped");
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        let ov = t.overlay();
        assert_eq!(ov.bars, [bar(0, 2.0), bar(60, 4.0)]);
        assert_eq!(ov.seq, 5);
        // After a reconnect the next bar is announced as a resync and the overlay restarts.
        t.on_upstream(up(UpstreamEvent::Disconnected));
        t.on_upstream(up(UpstreamEvent::Reconnected));
        t.on_upstream(up(UpstreamEvent::Bar(bar(180, 5.0))));
        let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match &e.kind {
                EventKind::Status(TopicStatus::Stale { .. }) => "stale",
                EventKind::Status(TopicStatus::Live) => "live",
                EventKind::Resync => "resync",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["stale", "live", "resync"]);
        assert_eq!(t.overlay().bars, [bar(180, 5.0)]);
    }

    #[test]
    fn trades_dedupe_and_resync_after_reconnect() {
        let cfg = HubConfig { trades_kept: 3, ..HubConfig::default() };
        let t = TradesTopic::new(InstrumentId::new("BTC"), &cfg);
        let tr = |tid: u64| Trade { px: 1.0, sz: 1.0, side: tr_proto::Side::Buy, time_ms: tid as i64, tid };
        let mut rx = t.subscribe();
        t.on_upstream(up(UpstreamEvent::Trades(vec![tr(2), tr(1)])));
        t.on_upstream(up(UpstreamEvent::Trades(vec![tr(3)])));
        let (snap, seq, _) = t.snapshot();
        assert_eq!((snap.iter().map(|x| x.tid).collect::<Vec<_>>(), seq), (vec![1, 2, 3], 2));
        t.on_upstream(up(UpstreamEvent::Reconnected));
        // The resubscription's history overlaps: only tid 4 is new, and it arrives as a resync.
        t.on_upstream(up(UpstreamEvent::Trades(vec![tr(2), tr(3), tr(4)])));
        assert_eq!(t.snapshot().0.iter().map(|x| x.tid).collect::<Vec<_>>(), vec![2, 3, 4]);
        let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match &e.kind {
                EventKind::Data(v) => v.iter().map(|x| x.tid).sum::<u64>() as i64,
                EventKind::Resync => -1,
                EventKind::Status(_) => -2,
            })
            .collect();
        assert_eq!(kinds, [3, 3, -1]);
    }

    #[test]
    fn merging_live_bars_over_history() {
        let hist = vec![bar(0, 1.0), bar(60, 2.0), bar(120, 3.0)];
        let merged = merge_bars(hist.clone(), &[bar(120, 3.5), bar(180, 4.0)], 10);
        assert_eq!(
            merged.iter().map(|b| (b.ts, b.close)).collect::<Vec<_>>(),
            [(0, 1.0), (60, 2.0), (120, 3.5), (180, 4.0)]
        );
        assert_eq!(merge_bars(hist, &[bar(180, 4.0)], 2).len(), 2);
        assert_eq!(merge_bars(vec![], &[bar(0, 1.0)], 5), [bar(0, 1.0)]);
    }

    #[test]
    fn mids_only_bump_what_changed() {
        let m = MidsTable::default();
        let rx = m.subscribe();
        let both = || vec![("BTC".into(), 1.0), ("ETH".into(), 2.5)];
        m.apply("", Instant::now(), 1, vec![("BTC".into(), 1.0), ("ETH".into(), 2.0)]);
        m.apply("", Instant::now(), 2, both());
        assert_eq!(*rx.borrow(), 2);
        assert_eq!(m.get("BTC").unwrap().stamp, 1);
        assert_eq!(m.get("ETH").unwrap().stamp, 2);
        m.apply("", Instant::now(), 3, both());
        assert_eq!(*rx.borrow(), 2, "no change, no new version");
    }

    #[test]
    fn coins_missing_from_their_venue_feed_are_dropped() {
        let m = MidsTable::default();
        m.apply("", Instant::now(), 1, vec![("BTC".into(), 1.0), ("OLD".into(), 2.0)]);
        m.apply("xyz", Instant::now(), 1, vec![("xyz:TSLA".into(), 400.0)]);
        // Each message carries the venue's full set: OLD was delisted.
        m.apply("", Instant::now(), 2, vec![("BTC".into(), 1.0)]);
        let mut coins = m.coins();
        coins.sort();
        assert_eq!(coins, ["BTC", "xyz:TSLA"], "other venues are untouched");
        assert_eq!(&*m.get("xyz:TSLA").unwrap().venue, "xyz");
    }

    #[test]
    fn venue_outages_are_visible() {
        let m = MidsTable::default();
        let rx = m.subscribe();
        m.apply("xyz", Instant::now(), 1, vec![("xyz:TSLA".into(), 400.0)]);
        let v0 = *rx.borrow();
        m.venue_event("xyz", 7, &UpstreamEvent::Disconnected);
        assert_eq!(m.venue("xyz"), VenueState { status: TopicStatus::Stale { since_ms: 7 }, epoch: 0 });
        m.venue_event("xyz", 8, &UpstreamEvent::Reconnected);
        assert_eq!(m.venue("xyz").epoch, 1);
        assert!(matches!(m.venue("xyz").status, TopicStatus::Stale { .. }), "stale until data arrives");
        // The first message after reconnecting makes it live, even with no price changes.
        m.apply("xyz", Instant::now(), 9, vec![("xyz:TSLA".into(), 400.0)]);
        assert_eq!(m.venue("xyz"), VenueState { status: TopicStatus::Live, epoch: 1 });
        assert_eq!(*rx.borrow(), v0 + 3);
        assert_eq!(m.venue("flx"), VenueState::default());
    }
}
