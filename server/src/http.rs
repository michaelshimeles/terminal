//! The gateway's HTTP side: the client websocket at `/ws`, and `/healthz`, `/readyz`,
//! `/metrics` (Prometheus text) and `/stats` (JSON) for operators. With an operator token set,
//! those two answer 404 to requests without it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Serialize;
use tokio::sync::watch;

use crate::access::{self, PerIp, Secret};
use crate::metrics::{Metrics, Quantiles, TopicKind};
use crate::provider::UpstreamStats;
use crate::session::{self, SessionConfig};
use crate::store::{Hub, TopicCounts};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub sessions: Arc<SessionConfig>,
    pub max_clients: usize,
    /// Becomes true when the gateway shuts down; every session closes.
    closing: watch::Receiver<bool>,
    next_session: Arc<AtomicU64>,
    started: Instant,
    access: Arc<Access>,
}

/// Who may use the gateway. Open by default: no token, no per-address cap.
#[derive(Default)]
pub struct Access {
    /// Required, as a bearer token, by `/stats` and `/metrics`.
    pub metrics_token: Option<Secret>,
    pub per_ip: PerIp,
    /// The header the edge puts the client's address in.
    pub client_ip_header: Option<HeaderName>,
    /// Whether the first client connection has been looked at (see [`note_forwarding`]).
    noted: AtomicBool,
}

impl Access {
    pub fn new(metrics_token: Option<Secret>, max_per_ip: usize, client_ip_header: Option<HeaderName>) -> Self {
        Self { metrics_token, per_ip: PerIp::new(max_per_ip), client_ip_header, noted: AtomicBool::new(false) }
    }
}

impl AppState {
    pub fn new(hub: Arc<Hub>, sessions: SessionConfig, max_clients: usize, closing: watch::Receiver<bool>) -> Self {
        Self {
            hub,
            sessions: Arc::new(sessions),
            max_clients,
            closing,
            next_session: Arc::new(AtomicU64::new(1)),
            started: Instant::now(),
            access: Arc::default(),
        }
    }

    pub fn with_access(mut self, access: Access) -> Self {
        self.access = Arc::new(access);
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws))
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/stats", get(stats_json))
        .with_state(state)
}

async fn ws(
    State(st): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if st.hub.metrics.clients.load(Ordering::Relaxed) >= st.max_clients as i64 {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many clients\n").into_response();
    }
    let a = &st.access;
    note_forwarding(a, &headers);
    let peer = peer.map_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), |Extension(ConnectInfo(p))| p);
    let Some(slot) = a.per_ip.admit(access::client_ip(peer, a.client_ip_header.as_ref(), &headers)) else {
        tracing::debug!("refused a client: too many connections from its address");
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections from your address\n").into_response();
    };
    let id = st.next_session.fetch_add(1, Ordering::Relaxed);
    upgrade.max_message_size(64 * 1024).on_upgrade(move |socket| async move {
        session::run(socket, st.hub, st.sessions, id, st.closing).await;
        drop(slot);
    })
}

/// Log once whether client connections arrive with the client-address header, so an operator
/// can tell whether `TR_CLIENT_IP_HEADER` and a per-address cap will work behind their edge.
/// Only whether the header is there, never what it holds.
fn note_forwarding(a: &Access, headers: &HeaderMap) {
    if a.noted.swap(true, Ordering::Relaxed) {
        return;
    }
    let name = a.client_ip_header.clone().unwrap_or(HeaderName::from_static("x-forwarded-for"));
    let seen = if headers.contains_key(&name) { "carries" } else { "has no" };
    tracing::info!("the first client connection {seen} a {name} header");
}

/// `/stats` and `/metrics` as if they weren't there, unless the request carries the token.
fn hidden(st: &AppState, headers: &HeaderMap) -> Option<Response> {
    (!access::operator(st.access.metrics_token.as_ref(), headers)).then(|| StatusCode::NOT_FOUND.into_response())
}

async fn readyz(State(st): State<AppState>) -> Response {
    let up = st.hub.provider().stats().connections_up;
    if up > 0 && st.hub.is_ready() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {up} upstream connections up, {} mids\n", st.hub.mids.len()),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub uptime_s: u64,
    pub provider: &'static str,
    pub attribution: &'static str,
    pub upstream: UpstreamStats,
    pub upstream_messages: u64,
    pub upstream_bytes: u64,
    pub upstream_reconnects: u64,
    pub upstream_rejected: u64,
    pub rest_requests: u64,
    pub rest_errors: u64,
    pub topics: TopicCounts,
    pub clients: i64,
    pub clients_total: u64,
    pub client_subscriptions: i64,
    pub frames_sent: u64,
    pub bytes_sent: u64,
    pub resyncs: u64,
    pub slow_consumer_closes: u64,
    /// Upstream frame received → client frame written, per topic, in microseconds.
    pub latency_us: BTreeMap<&'static str, Quantiles>,
}

pub fn stats(st: &AppState) -> Stats {
    let m = &st.hub.metrics;
    let p = st.hub.provider();
    Stats {
        uptime_s: st.started.elapsed().as_secs(),
        provider: p.id().0,
        attribution: p.display_policy().attribution,
        upstream: p.stats(),
        upstream_messages: Metrics::get(&m.upstream_messages),
        upstream_bytes: Metrics::get(&m.upstream_bytes),
        upstream_reconnects: Metrics::get(&m.upstream_reconnects),
        upstream_rejected: Metrics::get(&m.upstream_rejected),
        rest_requests: Metrics::get(&m.rest_requests),
        rest_errors: Metrics::get(&m.rest_errors),
        topics: st.hub.topic_counts(),
        clients: m.clients.load(Ordering::Relaxed),
        clients_total: Metrics::get(&m.clients_total),
        client_subscriptions: m.client_subscriptions.load(Ordering::Relaxed),
        frames_sent: Metrics::get(&m.frames_sent),
        bytes_sent: Metrics::get(&m.bytes_sent),
        resyncs: Metrics::get(&m.resyncs),
        slow_consumer_closes: Metrics::get(&m.slow_consumer_closes),
        latency_us: TopicKind::ALL.iter().map(|k| (k.label(), m.latency_for(*k).quantiles())).collect(),
    }
}

async fn stats_json(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = hidden(&st, &headers) {
        return r;
    }
    axum::Json(stats(&st)).into_response()
}

async fn metrics(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = hidden(&st, &headers) {
        return r;
    }
    let s = stats(&st);
    let mut o = String::new();
    let mut put = |name: &str, kind: &str, help: &str, rows: &[(String, f64)]| {
        let _ = writeln!(o, "# HELP tr_{name} {help}\n# TYPE tr_{name} {kind}");
        for (labels, v) in rows {
            let _ = writeln!(o, "tr_{name}{labels} {v}");
        }
    };
    let one = |v: f64| vec![(String::new(), v)];
    let u = &s.upstream;
    put(
        "upstream_connections",
        "gauge",
        "Upstream websocket connections",
        &[
            ("{state=\"open\"}".into(), u.connections_open as f64),
            ("{state=\"up\"}".into(), u.connections_up as f64),
            ("{state=\"budget\"}".into(), u.max_connections as f64),
        ],
    );
    let mut subs: Vec<(String, f64)> =
        u.by_kind.iter().map(|(k, n)| (format!("{{kind=\"{k}\"}}"), *n as f64)).collect();
    subs.push(("{kind=\"budget\"}".into(), u.max_subscriptions as f64));
    put("upstream_subscriptions", "gauge", "Upstream subscriptions by kind", &subs);
    put(
        "upstream_messages_sent_last_min",
        "gauge",
        "Messages sent upstream in the last minute",
        &one(u.messages_sent_last_min as f64),
    );
    put(
        "rest_weight_last_min",
        "gauge",
        "Info endpoint weight spent in the last minute",
        &one(u.rest_weight_last_min as f64),
    );
    put("upstream_messages_total", "counter", "Upstream messages received", &one(s.upstream_messages as f64));
    put("upstream_bytes_total", "counter", "Upstream bytes received", &one(s.upstream_bytes as f64));
    put("upstream_reconnects_total", "counter", "Upstream reconnects", &one(s.upstream_reconnects as f64));
    put("upstream_rejected_total", "counter", "Upstream subscriptions refused", &one(s.upstream_rejected as f64));
    put("rest_requests_total", "counter", "Info endpoint requests", &one(s.rest_requests as f64));
    put("rest_errors_total", "counter", "Info endpoint errors", &one(s.rest_errors as f64));
    let t = &s.topics;
    put(
        "topics",
        "gauge",
        "Hot-store topics",
        &[
            ("{kind=\"book\"}".into(), t.book as f64),
            ("{kind=\"trades\"}".into(), t.trades as f64),
            ("{kind=\"bars\"}".into(), t.bars as f64),
            ("{kind=\"lingering\"}".into(), t.lingering as f64),
            ("{kind=\"mids\"}".into(), t.mids as f64),
        ],
    );
    put("clients", "gauge", "Connected clients", &one(s.clients as f64));
    put("client_subscriptions", "gauge", "Client subscriptions", &one(s.client_subscriptions as f64));
    put("frames_sent_total", "counter", "Frames sent to clients", &one(s.frames_sent as f64));
    put("bytes_sent_total", "counter", "Bytes sent to clients", &one(s.bytes_sent as f64));
    put("resyncs_total", "counter", "Resyncs sent to clients", &one(s.resyncs as f64));
    put(
        "slow_consumer_closes_total",
        "counter",
        "Clients closed for falling behind",
        &one(s.slow_consumer_closes as f64),
    );
    let mut lat = Vec::new();
    for (topic, q) in &s.latency_us {
        for (quantile, v) in [("0.5", q.p50_us), ("0.9", q.p90_us), ("0.99", q.p99_us)] {
            lat.push((format!("{{topic=\"{topic}\",quantile=\"{quantile}\"}}"), f64::from(v)));
        }
    }
    put("server_latency_us", "summary", "Upstream frame received to client frame written, microseconds", &lat);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], o).into_response()
}
