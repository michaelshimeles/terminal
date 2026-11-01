//! Background worker. All network, SQLite, indicator and LOD-pyramid work runs on a
//! tokio runtime; the UI thread only sends [`Request`]s and drains [`Response`]s, so it
//! never blocks.
//!
//! In the browser (`wasm32`) the same requests run as tasks on the page's event loop
//! ([`tr_data::rt`]). There is no bar cache or Congress ingest there, and CPU work such as
//! indicators runs inline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::sync::Mutex;
use tr_chart::gpu::{LineKind, LineRef};
use tr_chart::scene::{Overlay, SeriesGpu, line_ref, marker_ref};
use tr_congress::db::{Alert, PoliticianSummary};
use tr_congress::model::NewTradeAlert;
use tr_congress::performance::{self, Performance, TradeReturn};
#[cfg(not(target_arch = "wasm32"))]
use tr_congress::refresh::{self, HttpSource, RefreshConfig};
use tr_congress::{Db, Politician, Trade};
use tr_core::indicators::registry::{Input, Plot, Tone};
use tr_core::{Bar, Security, Series, Timeframe};
#[cfg(not(target_arch = "wasm32"))]
use tr_data::BarCache;
use tr_data::{Provider, Quote, SearchHit, YieldCurves, rt};

use crate::command::IndicatorSpec;
use crate::theme;

pub type PaneId = u64;

/// Minutes between Congress disclosure checks (`TR_CONGRESS_REFRESH_MIN`; 0 disables).
pub const CONGRESS_REFRESH_MIN: u64 = 30;

/// The least time between a chart's reloads after its provider ended the live feed.
const RELOAD_AFTER_FEED_ENDS: Duration = Duration::from_secs(5);

pub enum Request {
    /// Load the brokerage portfolio through SnapTrade (`None` = the simulated sample).
    Portfolio(Option<tr_broker::Credentials>),
    /// Register a SnapTrade user under the partner keys.
    BrokerRegister {
        creds: tr_broker::Credentials,
        user_id: String,
    },
    /// Generate a read-only Connection Portal link (optionally to reconnect a connection).
    BrokerPortal {
        creds: tr_broker::Credentials,
        dark: bool,
        reconnect: Option<String>,
    },
    /// Remove a brokerage connection at the user's explicit request.
    BrokerDisconnect {
        creds: tr_broker::Credentials,
        id: String,
    },
    /// Option chain for one expiration (`None` = nearest).
    Options {
        pane: PaneId,
        symbol: String,
        expiration: Option<i64>,
    },
    /// Headlines for a market key (`None` = general market news).
    News {
        pane: PaneId,
        symbol: Option<String>,
    },
    OrderBook {
        pane: PaneId,
        symbol: String,
        depth: usize,
    },
    History {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        limit: usize,
    },
    /// Prepend older history to `existing`.
    MoreHistory {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        existing: Arc<Vec<Bar>>,
        limit: usize,
    },
    Indicators {
        pane: PaneId,
        generation: u64,
        bars: Arc<Vec<Bar>>,
        tf: Timeframe,
        specs: Vec<IndicatorSpec>,
        /// Colours are baked into overlays, so the UI passes its current palette.
        palette: theme::Palette,
    },
    Compare {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        base: Arc<Vec<Bar>>,
    },
    Markers {
        pane: PaneId,
        symbol: String,
    },
    Quotes(Vec<String>),
    Latest {
        limit: usize,
    },
    TickerTrades {
        pane: PaneId,
        symbol: String,
    },
    Profile {
        pane: PaneId,
        query: String,
    },
    SearchPoliticians(String),
    Track {
        query: String,
        on: bool,
    },
    Alerts,
    MarkAlertsSeen,
    RefreshCongress,
    /// A pane closed or changed: stop its live feed and drop its series.
    Forget(PaneId),
    YieldCurves,
    Search(String),
    /// Order entry (see `app::trading`); only sent once trading is enabled.
    Trade(TradeRequest),
}

/// A trading call through SnapTrade. Previews never place anything; placing takes a preview
/// the user confirmed.
pub enum TradeRequest {
    Preview {
        creds: tr_broker::Credentials,
        req: tr_broker::trading::OrderRequest,
        ticket: u64,
    },
    Place {
        creds: tr_broker::Credentials,
        preview: Box<tr_broker::trading::OrderPreview>,
        ticket: u64,
    },
    Cancel {
        creds: tr_broker::Credentials,
        account: String,
        order: String,
    },
    /// Connection Portal link requesting trading access.
    Portal {
        creds: tr_broker::Credentials,
        dark: bool,
    },
    /// Which brokerage connections can trade (brokerage support and connection access): one
    /// `/authorizations` request, however many accounts there are.
    Capabilities {
        creds: tr_broker::Credentials,
    },
}

pub enum TradeResponse {
    Preview {
        ticket: u64,
        result: Result<Box<tr_broker::trading::OrderPreview>, String>,
    },
    Placed {
        ticket: u64,
        preview: Box<tr_broker::trading::OrderPreview>,
        result: Result<Option<Box<tr_broker::Order>>, tr_broker::trading::PlaceError>,
    },
    Cancelled {
        account: String,
        order: String,
        result: Result<(), String>,
    },
    Portal(Result<String, String>),
    /// Capabilities by connection (SnapTrade authorization) id.
    Capabilities(Result<std::collections::HashMap<String, tr_broker::trading::Capability>, CapsError>),
}

/// Why trading access could not be read. A rate limit is reported apart from real errors so
/// the app can back off quietly instead of showing an error.
#[derive(Debug, Clone, PartialEq)]
pub struct CapsError {
    pub message: String,
    pub throttled: bool,
    pub retry_after: Option<Duration>,
}

pub struct BuiltIndicator {
    pub spec: IndicatorSpec,
    pub overlays: Vec<(String, Overlay)>,
}

pub struct Profile {
    pub politician: Politician,
    pub trades: Vec<Trade>,
    pub perf: Performance,
    pub returns: Vec<TradeReturn>,
    pub tracked: bool,
}

pub enum Response {
    Portfolio(Result<Box<tr_broker::Portfolio>, String>),
    /// `(userId, userSecret)` for a newly registered SnapTrade user.
    BrokerRegistered(Result<(String, String), String>),
    BrokerPortal(Result<String, String>),
    /// The connection id removed, or why not.
    BrokerDisconnected(Result<String, String>),
    Options {
        pane: PaneId,
        symbol: String,
        chain: Result<Box<tr_data::OptionChain>, String>,
    },
    News {
        pane: PaneId,
        symbol: Option<String>,
        items: Result<Vec<tr_data::NewsItem>, String>,
    },
    OrderBook {
        pane: PaneId,
        symbol: String,
        book: Result<tr_data::OrderBook, String>,
    },
    History {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        bars: Arc<Vec<Bar>>,
        gpu: SeriesGpu,
        /// Number of bars prepended (paging), 0 otherwise.
        prepended: usize,
        from_cache: bool,
        exhausted: bool,
        /// A full load from the provider. Its `exhausted` replaces the chart's, which may have
        /// come from the source the chart showed before.
        full: bool,
        /// A live-feed update of the latest bar.
        live: bool,
    },
    HistoryError {
        pane: PaneId,
        generation: u64,
        error: String,
    },
    Indicators {
        pane: PaneId,
        generation: u64,
        built: Vec<BuiltIndicator>,
    },
    Compare {
        pane: PaneId,
        generation: u64,
        symbol: String,
        line: LineRef,
    },
    Markers {
        pane: PaneId,
        symbol: String,
        trades: Vec<Trade>,
    },
    Quotes(Vec<Quote>),
    Latest(Vec<Trade>),
    TickerTrades {
        pane: PaneId,
        symbol: String,
        trades: Vec<Trade>,
    },
    Profile {
        pane: PaneId,
        profile: Option<Box<Profile>>,
        query: String,
    },
    Politicians(Vec<PoliticianSummary>),
    Alerts(Vec<Alert>),
    // Only the desktop's Congress ingest raises these.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    NewAlerts(Vec<NewTradeAlert>),
    Status(String),
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    CongressRefreshed(String),
    Curves(Box<YieldCurves>),
    SearchHits {
        query: String,
        hits: Vec<SearchHit>,
    },
    Trade(Box<TradeResponse>),
}

pub struct Worker {
    tx: tokio::sync::mpsc::UnboundedSender<Request>,
    pub rx: mpsc::Receiver<Response>,
    #[cfg(not(target_arch = "wasm32"))]
    _rt: tokio::runtime::Runtime,
}

/// Bars per pane tagged with the load generation they belong to.
type SeriesMap = HashMap<PaneId, (u64, Arc<Vec<Bar>>)>;

struct Shared {
    provider: Provider,
    /// Authoritative bars per chart pane (history, paging and live ticks all merge here),
    /// tagged with the pane's load generation.
    series: std::sync::Mutex<SeriesMap>,
    /// Counts each chart pane's reloads for a changed data source, up once as one starts and again
    /// when it's done (odd while it runs), so a page of older bars that spans one is dropped.
    sources: std::sync::Mutex<HashMap<PaneId, u64>>,
    #[cfg(not(target_arch = "wasm32"))]
    cache: std::sync::Mutex<Option<BarCache>>,
    db: Mutex<Db>,
    out: mpsc::Sender<Response>,
    ctx: egui::Context,
}

impl Shared {
    fn send(&self, r: Response) {
        let _ = self.out.send(r);
        self.ctx.request_repaint();
    }
}

/// Where saved files live. In the browser this is the key prefix in `localStorage` (see
/// [`crate::storage`]). The first call moves files saved under the app's old name, terminal-rs,
/// to the new one ([`crate::storage::migrate`]).
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Unit tests never read, write or move the real folder.
        if cfg!(test) {
            return std::env::temp_dir().join(format!("tr-test-data-{}", std::process::id()));
        }
        let parent = if cfg!(target_arch = "wasm32") {
            PathBuf::new()
        } else if let Some(d) = std::env::var_os("TR_DATA_DIR") {
            return PathBuf::from(d);
        } else {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            if cfg!(target_os = "macos") {
                home.join("Library/Application Support")
            } else {
                std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"))
            }
        };
        crate::storage::migrate(&parent.join("terminal-rs"), &parent.join("terminal"))
    })
    .clone()
}

impl Worker {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn spawn(ctx: egui::Context, provider: Provider) -> anyhow::Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let db = Db::open(dir.join("congress.sqlite3")).or_else(|e| {
            tracing::warn!("congress db unavailable ({e:#}); using in-memory store");
            Db::open_in_memory()
        })?;
        let cache =
            BarCache::open(dir.join("bars.sqlite3")).map_err(|e| tracing::warn!("bar cache unavailable: {e:#}")).ok();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("tr-worker")
            .build()?;
        let (tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (out, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            provider,
            series: Default::default(),
            sources: Default::default(),
            cache: std::sync::Mutex::new(cache),
            db: Mutex::new(db),
            out,
            ctx,
        });

        // Request dispatcher: every request runs as its own task.
        let s = shared.clone();
        rt.spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let s = s.clone();
                tokio::spawn(async move { handle(&s, req).await });
            }
        });

        // Scheduled congressional disclosure refresh.
        let s = shared.clone();
        rt.spawn(async move {
            let minutes: u64 = std::env::var("TR_CONGRESS_REFRESH_MIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(CONGRESS_REFRESH_MIN);
            if minutes == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut tick = tokio::time::interval(Duration::from_secs(minutes * 60));
            loop {
                tick.tick().await;
                congress_refresh(&s).await;
            }
        });
        Ok(Self { tx, rx, _rt: rt })
    }

    /// The browser worker: tasks on the page's event loop, no disk cache, an empty Congress store.
    #[cfg(target_arch = "wasm32")]
    pub fn spawn(ctx: egui::Context, provider: Provider) -> anyhow::Result<Self> {
        let (tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (out, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            provider,
            series: Default::default(),
            sources: Default::default(),
            db: Mutex::new(Db::open_in_memory()?),
            out,
            ctx,
        });
        rt::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let s = shared.clone();
                rt::spawn(async move { handle(&s, req).await });
            }
        });
        Ok(Self { tx, rx })
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn congress_refresh(s: &Shared) {
    s.send(Response::Status("Refreshing congressional disclosures (Senate eFD + House Clerk)…".into()));
    let source = match HttpSource::new() {
        Ok(src) => src,
        Err(e) => return s.send(Response::Status(format!("congress refresh failed: {e:#}"))),
    };
    let cfg = RefreshConfig::incremental(&*s.db.lock().await);
    match refresh::refresh(&source, &s.db, &cfg).await {
        Ok(report) => {
            if !report.alerts.is_empty() {
                s.send(Response::NewAlerts(report.alerts.clone()));
            }
            s.send(Response::CongressRefreshed(report.summary()));
        }
        Err(e) => s.send(Response::Status(format!("congress refresh failed: {e:#}"))),
    }
}

// TODO(web proxy): Senate eFD and the House Clerk don't allow cross-origin requests.
#[cfg(target_arch = "wasm32")]
async fn congress_refresh(s: &Shared) {
    s.send(Response::Status(crate::congress_view::WEB_NOTE.into()));
}

/// Bars saved by an earlier session, to paint before the network answers.
#[cfg(not(target_arch = "wasm32"))]
fn cached_bars(s: &Shared, symbol: &str, tf: Timeframe, limit: usize) -> Option<Vec<Bar>> {
    s.cache.lock().ok().and_then(|c| c.as_ref().and_then(|c| c.load(symbol, tf, limit).ok()))
}

#[cfg(not(target_arch = "wasm32"))]
fn cache_bars(s: &Shared, symbol: &str, tf: Timeframe, bars: &[Bar]) {
    if let Ok(mut c) = s.cache.lock()
        && let Some(c) = c.as_mut()
    {
        let _ = c.store(symbol, tf, bars);
    }
}

#[cfg(target_arch = "wasm32")]
fn cached_bars(_: &Shared, _: &str, _: Timeframe, _: usize) -> Option<Vec<Bar>> {
    None
}

#[cfg(target_arch = "wasm32")]
fn cache_bars(_: &Shared, _: &str, _: Timeframe, _: &[Bar]) {}

fn overlay(values: &[f64], color: egui::Color32, kind: LineKind) -> Overlay {
    let line = if kind == LineKind::Dots { marker_ref(values) } else { line_ref(values) };
    Overlay { line, color: theme::rgba(color), kind, scale: 1.0 }
}

/// Compute every indicator on `bars`. The `i`-th indicator takes the `i`-th series colour;
/// its signal lines, outer bands and up/down lines take theme roles.
pub fn build_indicators(
    bars: &[Bar],
    tf: Timeframe,
    specs: &[IndicatorSpec],
    palette: &theme::Palette,
) -> Vec<BuiltIndicator> {
    let input = Input::new(bars, tf);
    let colors = palette.series.len();
    specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let main = palette.series[i % colors];
            let mut outputs = spec.compute(&input);
            // Columns first, so lines and markers draw over them.
            outputs.sort_by_key(|o| o.plot != Plot::Hist);
            let overlays = outputs
                .into_iter()
                .map(|o| {
                    let color = match o.tone {
                        Tone::Main => main,
                        Tone::Signal if main == palette.amber => palette.cyan,
                        Tone::Signal => palette.amber,
                        Tone::Dim => main.gamma_multiply(0.6),
                        Tone::Up => palette.up,
                        Tone::Down => palette.down,
                        Tone::Series(k) => palette.series[(i + 1 + k as usize) % colors],
                    };
                    let kind = match o.plot {
                        Plot::Line => LineKind::Line,
                        Plot::Hist => LineKind::Hist,
                        Plot::Dots => LineKind::Dots,
                        Plot::Steps => LineKind::Steps,
                    };
                    (o.name.to_string(), overlay(&o.values, color, kind))
                })
                .collect();
            BuiltIndicator { spec: spec.clone(), overlays }
        })
        .collect()
}

/// Align `other`'s closes to `base` timestamps (last close at or before each bar). The
/// chart rebases the result to the first visible bar every frame.
pub fn align_compare(base: &[Bar], other: &[Bar]) -> Vec<f64> {
    let mut out = vec![f64::NAN; base.len()];
    let mut j = 0;
    for (i, b) in base.iter().enumerate() {
        while j + 1 < other.len() && other[j + 1].ts <= b.ts {
            j += 1;
        }
        if let Some(o) = other.get(j).filter(|o| o.ts <= b.ts) {
            out[i] = o.close;
        }
    }
    out
}

fn default_limit(tf: Timeframe) -> usize {
    match tf {
        Timeframe::M1 => 5_000,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 => 5_000,
        Timeframe::H1 | Timeframe::H4 => 4_000,
        Timeframe::D1 => 6_000,
        Timeframe::W1 | Timeframe::MN1 => 2_000,
    }
}

/// Bars requested on first load; `TR_HISTORY_LIMIT` overrides (e.g. 1000000 with sample data).
pub fn initial_limit(tf: Timeframe) -> usize {
    std::env::var("TR_HISTORY_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| default_limit(tf))
}

async fn handle(s: &Arc<Shared>, req: Request) {
    match req {
        Request::History { pane, generation, symbol, tf, limit } => {
            if !claim_series(s, pane, generation) {
                return; // a newer load for this pane is already running
            }
            // Paint from the disk cache immediately, then refresh from the network.
            if let Some(bars) = cached_bars(s, &symbol, tf, limit).filter(|b| !b.is_empty()) {
                let bars = Arc::new(bars);
                let gpu = build_gpu(bars.clone()).await;
                s.send(Response::History {
                    pane,
                    generation,
                    symbol: symbol.clone(),
                    tf,
                    bars,
                    gpu,
                    prepended: 0,
                    from_cache: true,
                    exhausted: false,
                    full: false,
                    live: false,
                });
            }
            match s.provider.history(&symbol, tf, None, limit).await {
                Ok(bars) if !bars.is_empty() => {
                    cache_bars(s, &symbol, tf, &bars);
                    let exhausted = bars.len() < limit / 2;
                    let bars = Arc::new(bars);
                    if !store_series(s, pane, generation, bars.clone()) {
                        return; // superseded while loading
                    }
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol: symbol.clone(),
                        tf,
                        bars,
                        gpu,
                        prepended: 0,
                        from_cache: false,
                        exhausted,
                        full: true,
                        live: false,
                    });
                    start_live(s.clone(), pane, generation, symbol, tf, limit);
                }
                Ok(_) => s.send(Response::HistoryError { pane, generation, error: format!("no data for {symbol}") }),
                Err(e) => s.send(Response::HistoryError { pane, generation, error: format!("{e:#}") }),
            }
        }
        Request::MoreHistory { pane, generation, symbol, tf, existing, limit } => {
            // Page from the worker's copy (it may hold newer live bars than the UI's).
            let base = current_series(s, pane, generation).filter(|b| !b.is_empty()).unwrap_or(existing);
            let Some(first) = base.first().copied() else { return };
            // A page that overlaps a reload for another source is dropped. The reload answers the
            // pane, and the chart pages again from the new source.
            let source = source_switches(s, pane);
            if source % 2 == 1 {
                return;
            }
            match s.provider.history(&symbol, tf, Some(first.ts), limit).await {
                Ok(older) => {
                    let older: Vec<Bar> = older.into_iter().filter(|b| b.ts < first.ts).collect();
                    let prepended = older.len();
                    let bars = {
                        let mut map = s.series.lock().expect("series lock");
                        let Some((g, cur)) = map.get_mut(&pane).filter(|(g, _)| *g == generation) else { return };
                        let _ = g;
                        if source_switches(s, pane) != source {
                            return;
                        }
                        let mut merged = older;
                        merged.extend_from_slice(cur);
                        *cur = Arc::new(merged);
                        cur.clone()
                    };
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol,
                        tf,
                        bars,
                        gpu,
                        prepended,
                        from_cache: false,
                        exhausted: prepended == 0,
                        full: false,
                        live: false,
                    });
                }
                Err(e) if source_switches(s, pane) == source => {
                    s.send(Response::HistoryError { pane, generation, error: format!("{e:#}") })
                }
                Err(_) => {}
            }
        }
        Request::Indicators { pane, generation, bars, tf, specs, palette } => {
            let built = rt::blocking(move || build_indicators(&bars, tf, &specs, &palette)).await.unwrap_or_default();
            s.send(Response::Indicators { pane, generation, built });
        }
        Request::Compare { pane, generation, symbol, tf, base } => {
            let limit = base.len().max(100);
            match s.provider.history(&symbol, tf, None, limit).await {
                Ok(other) => {
                    let line = rt::blocking(move || line_ref(&align_compare(&base, &other))).await;
                    if let Some(line) = line {
                        s.send(Response::Compare { pane, generation, symbol, line });
                    }
                }
                Err(e) => s.send(Response::Status(format!("compare {symbol}: {e:#}"))),
            }
        }
        Request::Markers { pane, symbol } => {
            let sec = tr_data::security(&symbol);
            let trades = if sec.has_congress_trades() {
                s.db.lock().await.trades_for_ticker(&sec.ticker, 2_000).unwrap_or_default()
            } else {
                Vec::new()
            };
            s.send(Response::Markers { pane, symbol, trades });
        }
        Request::Quotes(symbols) => match s.provider.quotes(&symbols).await {
            Ok(q) => s.send(Response::Quotes(q)),
            Err(e) => s.send(Response::Status(format!("quotes: {e:#}"))),
        },
        Request::Latest { limit } => {
            let t = s.db.lock().await.latest_trades(limit).unwrap_or_default();
            s.send(Response::Latest(t));
        }
        Request::TickerTrades { pane, symbol } => {
            let ticker = Security::parse(&symbol).map(|x| x.ticker).unwrap_or_else(|| symbol.clone());
            let trades = s.db.lock().await.trades_for_ticker(&ticker, 1_000).unwrap_or_default();
            s.send(Response::TickerTrades { pane, symbol, trades });
        }
        Request::Profile { pane, query } => {
            let profile = profile(s, &query).await;
            s.send(Response::Profile { pane, profile: profile.map(Box::new), query });
        }
        Request::SearchPoliticians(q) => {
            let r = s.db.lock().await.search_politicians(&q, 500).unwrap_or_default();
            s.send(Response::Politicians(r));
        }
        Request::Track { query, on } => {
            let db = s.db.lock().await;
            let hit = resolve_politician(&db, &query);
            match hit {
                Some(p) => {
                    let _ = db.set_tracked(p.id, on);
                    let verb = if on { "Tracking" } else { "Stopped tracking" };
                    s.send(Response::Status(format!("{verb} {} — alerts fire when they file a new PTR", p.name)));
                }
                None => s.send(Response::Status(format!("No politician matches '{query}' yet (run REFRESH)"))),
            }
            let r = db.search_politicians("", 500).unwrap_or_default();
            s.send(Response::Politicians(r));
        }
        Request::Alerts => {
            let a = s.db.lock().await.alerts(false, 200).unwrap_or_default();
            s.send(Response::Alerts(a));
        }
        Request::MarkAlertsSeen => {
            let db = s.db.lock().await;
            let _ = db.mark_alerts_seen();
            s.send(Response::Alerts(db.alerts(false, 200).unwrap_or_default()));
        }
        Request::RefreshCongress => congress_refresh(s).await,
        Request::Forget(pane) => {
            s.series.lock().expect("series lock").remove(&pane);
        }
        Request::YieldCurves => match s.provider.yield_curves().await {
            Ok(c) => s.send(Response::Curves(Box::new(c))),
            Err(e) => s.send(Response::Status(format!("yield curve: {e:#}"))),
        },
        Request::Portfolio(creds) => {
            let result = match creds {
                None => Ok(tr_broker::sample::portfolio()),
                Some(c) => match tr_broker::SnapTrade::new(c) {
                    Ok(client) => client.portfolio().await,
                    Err(e) => Err(e),
                },
            };
            s.send(Response::Portfolio(result.map(Box::new).map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerRegister { creds, user_id } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.register_user(&user_id).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerRegistered(result.map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerPortal { creds, dark, reconnect } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.connection_portal_url(dark, reconnect.as_deref()).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerPortal(result.map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerDisconnect { creds, id } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.delete_connection(&id).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerDisconnected(result.map(|()| id).map_err(|e| format!("{e:#}"))));
        }
        Request::Options { pane, symbol, expiration } => {
            let chain = s.provider.options(&symbol, expiration).await.map(Box::new).map_err(|e| format!("{e:#}"));
            s.send(Response::Options { pane, symbol, chain });
        }
        Request::News { pane, symbol } => {
            let items = s.provider.news(symbol.as_deref()).await.map_err(|e| format!("{e:#}"));
            s.send(Response::News { pane, symbol, items });
        }
        Request::OrderBook { pane, symbol, depth } => {
            let book = s.provider.order_book(&symbol, depth).await.map_err(|e| format!("{e:#}"));
            s.send(Response::OrderBook { pane, symbol, book });
        }
        Request::Search(query) => {
            let hits = s.provider.search(&query).await.unwrap_or_default();
            s.send(Response::SearchHits { query, hits });
        }
        Request::Trade(t) => s.send(Response::Trade(Box::new(trade(t).await))),
    }
}

async fn trade(t: TradeRequest) -> TradeResponse {
    let err = |e: anyhow::Error| format!("{e:#}");
    let client = |creds| tr_broker::SnapTrade::new(creds).map_err(err);
    match t {
        TradeRequest::Preview { creds, req, ticket } => {
            let result = match client(creds) {
                Ok(c) => c.order_impact(&req).await.map(Box::new).map_err(err),
                Err(e) => Err(e),
            };
            TradeResponse::Preview { ticket, result }
        }
        TradeRequest::Place { creds, preview, ticket } => {
            let result = match client(creds) {
                Ok(c) => c.place_previewed(&preview).await.map(|o| o.map(Box::new)),
                Err(e) => Err(tr_broker::trading::PlaceError::NotSent(e)),
            };
            TradeResponse::Placed { ticket, preview, result }
        }
        TradeRequest::Cancel { creds, account, order } => {
            let result = match client(creds) {
                Ok(c) => c.cancel_order(&account, &order).await.map_err(err),
                Err(e) => Err(e),
            };
            TradeResponse::Cancelled { account, order, result }
        }
        TradeRequest::Portal { creds, dark } => TradeResponse::Portal(match client(creds) {
            Ok(c) => c.trading_portal_url(dark).await.map_err(err),
            Err(e) => Err(e),
        }),
        TradeRequest::Capabilities { creds } => TradeResponse::Capabilities(match client(creds) {
            Ok(c) => c.connection_capabilities().await.map_err(|e| {
                let throttle = tr_broker::snaptrade::throttle(&e);
                CapsError { message: err(e), throttled: throttle.is_some(), retry_after: throttle.flatten() }
            }),
            Err(message) => Err(CapsError { message, throttled: false, retry_after: None }),
        }),
    }
}

/// Register a load for `generation`; generations only move forward, since request
/// tasks run concurrently and may start out of order.
fn claim_series(s: &Shared, pane: PaneId, generation: u64) -> bool {
    let mut map = s.series.lock().expect("series lock");
    match map.get(&pane) {
        Some((g, _)) if *g > generation => false,
        Some((g, _)) if *g == generation => true,
        _ => {
            map.insert(pane, (generation, Arc::new(Vec::new())));
            true
        }
    }
}

/// Store freshly loaded bars for a pane if `generation` is still the newest.
fn store_series(s: &Shared, pane: PaneId, generation: u64, bars: Arc<Vec<Bar>>) -> bool {
    let mut map = s.series.lock().expect("series lock");
    match map.get(&pane) {
        Some((g, _)) if *g > generation => false,
        _ => {
            map.insert(pane, (generation, bars));
            true
        }
    }
}

fn source_switches(s: &Shared, pane: PaneId) -> u64 {
    s.sources.lock().expect("sources lock").get(&pane).copied().unwrap_or(0)
}

fn current_series(s: &Shared, pane: PaneId, generation: u64) -> Option<Arc<Vec<Bar>>> {
    s.series.lock().expect("series lock").get(&pane).filter(|(g, _)| *g == generation).map(|(_, b)| b.clone())
}

/// Merge the provider's live feed (if any) into the pane's series, publishing at most
/// four updates a second. Stops when the pane reloads, closes or the feed ends.
/// What one live bar did to a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Applied {
    changed: bool,
    /// It went in before the latest bar (a feed filling a gap after an outage), so every
    /// series aligned to the bars by index (compare lines) has to be rebuilt.
    inserted: bool,
}

fn apply_live_bar(bars: &mut Vec<Bar>, bar: Bar) -> Applied {
    let (len, last) = (bars.len(), bars.last().map(|b| b.ts));
    let changed = Series::apply_live(bars, bar);
    Applied { changed, inserted: bars.len() > len && bars.last().map(|b| b.ts) == last }
}

fn start_live(s: Arc<Shared>, pane: PaneId, generation: u64, symbol: String, tf: Timeframe, limit: usize) {
    let Some(mut feed) = s.provider.live(&symbol, tf) else { return };
    rt::spawn(async move {
        let started = web_time::Instant::now();
        let mut tick = rt::interval(Duration::from_millis(250));
        let mut dirty = false;
        let mut inserted = false;
        let mut ended = false;
        loop {
            tokio::select! {
                bar = feed.recv() => {
                    let Some(bar) = bar else {
                        ended = true;
                        break;
                    };
                    let mut map = s.series.lock().expect("series lock");
                    match map.get_mut(&pane) {
                        Some((g, bars)) if *g == generation => {
                            let a = apply_live_bar(Arc::make_mut(bars), bar);
                            dirty |= a.changed;
                            inserted |= a.inserted;
                        }
                        _ => break,
                    }
                }
                _ = tick.tick() => {
                    if !dirty {
                        if current_series(&s, pane, generation).is_none() {
                            break;
                        }
                        continue;
                    }
                    dirty = false;
                    let Some(bars) = current_series(&s, pane, generation) else { break };
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol: symbol.clone(),
                        tf,
                        bars,
                        gpu,
                        prepended: 0,
                        from_cache: false,
                        exhausted: false,
                        full: false,
                        // A bar slotted into a gap shifts everything after it: send it as a
                        // refresh so the chart rebuilds its compare line and indicators.
                        live: !std::mem::take(&mut inserted),
                    });
                }
            }
        }
        tracing::debug!("live feed for pane {pane} ({symbol}) stopped");
        // A provider ends a feed the pane still shows when the chart's source changes (the Terminal
        // server went away or came back): reload the chart from wherever its data comes from now.
        if ended {
            rt::sleep(RELOAD_AFTER_FEED_ENDS.saturating_sub(started.elapsed())).await;
            if current_series(&s, pane, generation).is_some() {
                let switch = || *s.sources.lock().expect("sources lock").entry(pane).or_default() += 1;
                switch();
                handle(&s, Request::History { pane, generation, symbol, tf, limit }).await;
                switch();
            }
        }
    });
}

async fn build_gpu(bars: Arc<Vec<Bar>>) -> SeriesGpu {
    rt::blocking(move || SeriesGpu::build(&bars)).await.expect("pyramid build task")
}

fn resolve_politician(db: &Db, query: &str) -> Option<Politician> {
    if let Ok(id) = query.trim().parse::<i64>() {
        return db.politician(id).ok().flatten();
    }
    // Prefer an exact last-name hit, then substring matches by activity.
    let all = db.search_politicians(query.trim(), 50).ok()?;
    let q = query.trim().to_ascii_lowercase();
    all.iter()
        .find(|p| p.politician.name.to_ascii_lowercase().split_whitespace().last() == Some(q.as_str()))
        .or(all.first())
        .map(|p| p.politician.clone())
}

async fn profile(s: &Shared, query: &str) -> Option<Profile> {
    let (politician, trades, tracked) = {
        let db = s.db.lock().await;
        let p = resolve_politician(&db, query)?;
        let trades = db.trades_for_politician(p.id).ok()?;
        let tracked = db.is_tracked(p.id).unwrap_or(false);
        (p, trades, tracked)
    };
    // Daily closes for each traded ticker (bounded) to estimate performance.
    let mut tickers: Vec<String> = trades.iter().filter_map(|t| t.ticker.clone()).collect();
    tickers.sort();
    tickers.dedup();
    tickers.truncate(40);
    let oldest = trades.iter().map(|t| t.tx_date).min();
    let days = oldest.map(|d| (chrono::Utc::now().date_naive() - d).num_days().max(30) as usize + 10).unwrap_or(400);
    let mut prices: std::collections::HashMap<String, Vec<Bar>> = std::collections::HashMap::new();
    let provider = s.provider.clone();
    let fetches = tickers.into_iter().map(|t| {
        let provider = provider.clone();
        async move {
            let r = provider.history(&format!("{t} US Equity"), Timeframe::D1, None, days.min(4000)).await;
            (t, r)
        }
    });
    for (t, r) in futures_join(fetches).await {
        if let Ok(b) = r {
            prices.insert(t, b);
        }
    }
    let (perf, returns) = performance::estimate(
        &trades,
        |tk, date| {
            let bars = prices.get(tk)?;
            let ts = date.and_hms_opt(23, 59, 59)?.and_utc().timestamp();
            let i = bars.partition_point(|b| b.ts <= ts).checked_sub(1)?;
            Some(bars[i].close)
        },
        |tk| prices.get(tk).and_then(|b| b.last()).map(|b| b.close),
    );
    Some(Profile { politician, trades, perf, returns, tracked })
}

/// Run futures concurrently, at most 6 in flight (keeps data providers happy).
async fn futures_join<F, T>(futs: impl Iterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::StreamExt;
    futures::stream::iter(futs).buffered(6).collect().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_filling_a_gap_are_reported_as_inserted() {
        let b = |ts: i64| Bar::new(ts, 1.0, 1.0, 1.0, 1.0, 1.0);
        let mut bars = vec![b(0), b(60), b(240)];
        assert_eq!(apply_live_bar(&mut bars, b(300)), Applied { changed: true, inserted: false });
        assert_eq!(apply_live_bar(&mut bars, b(300)), Applied { changed: false, inserted: false });
        assert_eq!(apply_live_bar(&mut bars, b(120)), Applied { changed: true, inserted: true });
        assert_eq!(
            apply_live_bar(&mut bars, Bar::new(60, 1.0, 2.0, 1.0, 2.0, 1.0)),
            Applied { changed: true, inserted: false }
        );
        assert_eq!(bars.iter().map(|x| x.ts).collect::<Vec<_>>(), [0, 60, 120, 240, 300]);
    }

    #[test]
    fn compare_alignment_carries_last_close() {
        let base: Vec<Bar> = (0..5).map(|i| Bar::new(i * 10, 100.0, 100.0, 100.0, 100.0 + i as f64, 1.0)).collect();
        let other: Vec<Bar> = vec![Bar::new(5, 0.0, 0.0, 0.0, 50.0, 1.0), Bar::new(25, 0.0, 0.0, 0.0, 55.0, 1.0)];
        let a = align_compare(&base, &other);
        assert!(a[0].is_nan());
        assert_eq!(&a[1..], &[50.0, 50.0, 55.0, 55.0]);
    }

    #[test]
    fn indicators_build_expected_overlays() {
        let bars = tr_core::sample::generate("X", Timeframe::D1, 300, 1_790_000_000);
        let palette = crate::themes::terminal_light().palette(theme::DEFAULT_ACCENT);
        let specs: Vec<IndicatorSpec> =
            ["BB", "MACD", "VWAP", "PSAR", "PIVOT"].iter().map(|c| IndicatorSpec::new(c, &[]).unwrap()).collect();
        let built = build_indicators(&bars, Timeframe::D1, &specs, &palette);
        assert_eq!(built[0].overlays.len(), 3);
        // The histogram draws first, under the MACD and signal lines.
        assert_eq!(built[1].overlays[0].1.kind, LineKind::Hist);
        assert_ne!(built[1].overlays[2].1.color, built[1].overlays[1].1.color, "signal stands out from MACD");
        assert_eq!(built[2].overlays.len(), 1);
        assert!(built[3].overlays.iter().all(|(_, o)| o.kind == LineKind::Dots));
        assert_eq!(built[3].overlays[0].1.color, theme::rgba(palette.up));
        // Every pair of bars holding a marker keeps one when zoomed out.
        let up = &built[3].overlays[0].1.line.pyramid;
        let pairs = up.data[..up.offsets[1]].chunks(2).filter(|c| c.iter().any(|p| p[1].is_finite())).count();
        assert_eq!(up.data[up.offsets[1]..up.offsets[2]].iter().filter(|p| p[1].is_finite()).count(), pairs);
        assert!(built[4].overlays.iter().all(|(_, o)| o.kind == LineKind::Steps));
    }

    #[test]
    fn series_generations_only_move_forward() {
        let (out, _rx) = mpsc::channel();
        let s = Shared {
            provider: std::sync::Arc::new(tr_data::sample::SampleProvider::at(0, 10)),
            series: Default::default(),
            sources: Default::default(),
            cache: std::sync::Mutex::new(None),
            db: Mutex::new(Db::open_in_memory().unwrap()),
            out,
            ctx: egui::Context::default(),
        };
        // The newer load's task starts first; the older one must not take over.
        assert!(claim_series(&s, 1, 2));
        assert!(!claim_series(&s, 1, 1));
        assert!(!store_series(&s, 1, 1, Arc::new(vec![])));
        assert!(store_series(&s, 1, 2, Arc::new(vec![Bar::new(0, 1.0, 1.0, 1.0, 1.0, 1.0)])));
        assert_eq!(current_series(&s, 1, 2).unwrap().len(), 1);
        assert!(current_series(&s, 1, 1).is_none());
    }

    /// Bars whose close counts the loads. Reloads wait for `reload`, and older pages for `page`.
    /// The first chart's live feed ends at once, the way `RemoteMarkets` ends one when the chart's
    /// source changes.
    #[derive(Default)]
    struct SourceChanges {
        loads: std::sync::atomic::AtomicUsize,
        open: std::sync::Mutex<Vec<tokio::sync::mpsc::Sender<Bar>>>,
        reload: tokio::sync::Notify,
        page: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl tr_data::MarketData for SourceChanges {
        fn name(&self) -> &str {
            "test"
        }

        async fn history(&self, _: &str, _: Timeframe, end: Option<i64>, _: usize) -> anyhow::Result<Vec<Bar>> {
            if end.is_some() {
                self.page.notified().await;
                return Ok(vec![Bar::new(-60, 1.0, 1.0, 1.0, 1.0, 1.0)]);
            }
            let n = self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n > 1 {
                self.reload.notified().await;
            }
            Ok(vec![Bar::new(0, 1.0, 1.0, 1.0, n as f64, 1.0)])
        }

        fn live(&self, _: &str, _: Timeframe) -> Option<tr_data::LiveFeed> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            if self.loads.load(std::sync::atomic::Ordering::SeqCst) > 1 {
                self.open.lock().unwrap().push(tx);
            }
            Some(rx)
        }
    }

    /// A worker on `SourceChanges` with pane 1 loading `BTCUSD Curncy`, and its responses.
    async fn switching_chart() -> (Arc<Shared>, mpsc::Receiver<Response>, Arc<SourceChanges>) {
        let (out, rx) = mpsc::channel();
        let provider = Arc::new(SourceChanges::default());
        let s = Arc::new(Shared {
            provider: provider.clone(),
            series: Default::default(),
            sources: Default::default(),
            cache: std::sync::Mutex::new(None),
            db: Mutex::new(Db::open_in_memory().unwrap()),
            out,
            ctx: egui::Context::default(),
        });
        let symbol = "BTCUSD Curncy".to_string();
        handle(&s, Request::History { pane: 1, generation: 1, symbol, tf: Timeframe::D1, limit: 10 }).await;
        (s, rx, provider)
    }

    /// The last close of each chart update, until there are `n`.
    async fn closes(rx: &mpsc::Receiver<Response>, n: usize) -> Vec<f64> {
        let t0 = std::time::Instant::now();
        let mut closes = Vec::new();
        while closes.len() < n {
            assert!(t0.elapsed() < Duration::from_secs(15), "no reload: {closes:?}");
            match rx.try_recv() {
                Ok(Response::History { generation: 1, bars, .. }) => closes.push(bars.last().unwrap().close),
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        closes
    }

    #[tokio::test]
    async fn charts_reload_when_the_provider_ends_their_live_feed() {
        let t0 = std::time::Instant::now();
        let (_s, rx, provider) = switching_chart().await;
        provider.reload.notify_one();
        assert_eq!(closes(&rx, 2).await, [1.0, 2.0], "the reload replaced the chart");
        assert!(t0.elapsed() >= RELOAD_AFTER_FEED_ENDS, "not before the pause");
        assert_eq!(provider.loads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pages_that_overlap_a_source_switch_are_dropped() {
        let (s, rx, provider) = switching_chart().await;
        let more = || Request::MoreHistory {
            pane: 1,
            generation: 1,
            symbol: "BTCUSD Curncy".into(),
            tf: Timeframe::D1,
            existing: Arc::new(Vec::new()),
            limit: 10,
        };
        // One page starts before the switch, and its bars arrive after it.
        let before = tokio::spawn({
            let (s, req) = (s.clone(), more());
            async move { handle(&s, req).await }
        });
        let t0 = std::time::Instant::now();
        while source_switches(&s, 1) != 1 {
            assert!(t0.elapsed() < Duration::from_secs(15), "no reload");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Another starts while the chart reloads, and gives up at once.
        let during = tokio::time::timeout(Duration::from_secs(1), handle(&s, more())).await;
        assert!(during.is_ok(), "a page started during the reload went ahead");
        provider.reload.notify_one();
        assert_eq!(closes(&rx, 2).await, [1.0, 2.0]);
        provider.page.notify_one();
        before.await.unwrap();
        let ts: Vec<i64> = current_series(&s, 1, 1).unwrap().iter().map(|b| b.ts).collect();
        assert_eq!(ts, [0], "the old source's page joined the new source's bars");
        assert!(rx.try_iter().all(|r| !matches!(r, Response::History { prepended: 1.., .. })));
    }

    #[test]
    fn join_all_runs_everything() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let out = rt.block_on(futures_join((0..20).map(|i| async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            i * 2
        })));
        assert_eq!(out, (0..20).map(|i| i * 2).collect::<Vec<_>>());
    }
}
