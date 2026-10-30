//! The server-side source of market data (`docs/architecture/server.md` §3.1). One provider
//! per upstream. The gateway only talks to this trait, so a provider can be replaced (the
//! public-feed `HlPublicProvider` today, a node-backed `HlNodeProvider` later) without any
//! client or protocol change.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use tr_core::{Bar, Timeframe};
use tr_proto::{InstrumentKind, Level, Trade};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId(pub &'static str);

/// A provider's own name for an instrument. For Hyperliquid, the coin: `BTC`, `xyz:TSLA`,
/// `@107`. For [`Channel::Mids`] it names a venue instead (a perp dex; `""` is the core dex).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstrumentId(pub String);

impl InstrumentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Every mid price on one venue.
    Mids,
    Book,
    Trades,
    Bars(Timeframe),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Mids,
    Book,
    Trades,
    Bars,
}

/// What a provider can serve: channels × instrument kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub channels: Vec<ChannelKind>,
    pub kinds: Vec<InstrumentKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entitlement {
    Staff,
}

/// Display rights for a provider's data. The gateway enforces these, never the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPolicy {
    /// May the general public (any signed-in or anonymous user) see it?
    pub public_display: bool,
    /// If not public: the entitlement that unlocks it.
    pub entitlement: Option<Entitlement>,
    /// Required delay before showing it to users without the entitlement.
    pub delay: Option<Duration>,
    /// Attribution the UI must show.
    pub attribution: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentDef {
    pub id: InstrumentId,
    /// A perp dex (`""` is the core dex) or `spot`.
    pub venue: String,
    pub kind: InstrumentKind,
    /// The price 24 hours earlier, for change figures, when the provider has one.
    pub prev_day: Option<f64>,
}

/// One upstream event and when it arrived.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// When the frame carrying it arrived, for the server-added latency histogram.
    pub recv: Instant,
    /// The same moment as Unix milliseconds, sent to clients as the time the server saw it.
    pub recv_ms: i64,
    pub event: UpstreamEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamEvent {
    /// `(instrument, mid)` pairs that changed. The first message after (re)subscribing has all.
    Mids(Vec<(String, f64)>),
    Book {
        bids: Vec<Level>,
        asks: Vec<Level>,
        time_ms: i64,
    },
    /// New trades, oldest first. Right after (re)subscribing this holds recent history.
    Trades(Vec<Trade>),
    /// The latest state of the bar that `Bar::ts` opens.
    Bar(Bar),
    /// The upstream connection dropped. Data is stale until [`UpstreamEvent::Reconnected`].
    Disconnected,
    /// The upstream reconnected and resubscribed. The next data starts fresh, and anything sent
    /// in between was missed.
    Reconnected,
    /// The upstream refused this subscription; it gets no more data.
    Rejected(String),
}

/// Where a subscription's events go. Called on the provider's I/O task, so it must not block.
#[derive(Clone)]
pub struct UpstreamSink(Arc<dyn Fn(Upstream) + Send + Sync>);

impl UpstreamSink {
    pub fn new(f: impl Fn(Upstream) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn send(&self, ev: Upstream) {
        (self.0)(ev)
    }
}

/// Keeps an upstream subscription alive; dropping it unsubscribes.
pub struct UpstreamHandle(#[allow(dead_code)] Box<dyn Send + Sync>);

impl UpstreamHandle {
    pub fn new(guard: impl Send + Sync + 'static) -> Self {
        Self(Box::new(guard))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeError {
    /// At the provider's upstream subscription or connection budget.
    Capacity(String),
    UnknownInstrument(String),
    Unsupported(String),
}

impl fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubscribeError::Capacity(m) => write!(f, "upstream budget: {m}"),
            SubscribeError::UnknownInstrument(m) => write!(f, "unknown instrument {m}"),
            SubscribeError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for SubscribeError {}

/// A provider's upstream usage against its budgets, for `/stats` and `/metrics`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct UpstreamStats {
    pub connections_open: usize,
    pub connections_up: usize,
    pub max_connections: usize,
    pub subscriptions: usize,
    pub max_subscriptions: usize,
    pub by_kind: BTreeMap<&'static str, usize>,
    pub per_connection: Vec<usize>,
    /// Every upstream subscription, for example `l2Book:BTC` or `candle:xyz:TSLA:1m`.
    pub keys: Vec<String>,
    pub messages_sent_last_min: u32,
    pub messages_per_min: u32,
    pub connects_last_min: u32,
    pub rest_weight_last_min: u32,
    pub rest_weight_per_min: u32,
}

/// Server-side source of market data. The gateway asks it what it serves, fetches history from
/// it and opens streams on it; everything else (fan-out, sequencing, backpressure) is the
/// gateway's job.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> Capabilities;
    fn display_policy(&self) -> DisplayPolicy;
    fn stats(&self) -> UpstreamStats {
        UpstreamStats::default()
    }
    /// Close every upstream connection for good (gateway shutdown).
    fn close(&self) {}
    /// Instruments listed right now. Called periodically; delistings show up here.
    async fn instruments(&self) -> Result<Vec<InstrumentDef>>;
    /// Up to `limit` bars before `end` (Unix seconds; `None` = now), oldest first: the same
    /// contract as `MarketData::history`.
    async fn history(&self, inst: &InstrumentId, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>>;
    /// Start an upstream stream. Events go to `sink` until the handle is dropped.
    async fn subscribe(
        &self,
        ch: Channel,
        inst: &InstrumentId,
        sink: UpstreamSink,
    ) -> Result<UpstreamHandle, SubscribeError>;
}
