//! The gateway's HTTP side: the client websocket at `/ws`, and `/healthz`, `/readyz`,
//! `/metrics` (Prometheus text) and `/stats` (JSON) for operators.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;

use crate::metrics::{Metrics, Quantiles, TopicKind};
use crate::provider::UpstreamStats;
use crate::session::{self, SessionConfig};
use crate::store::{Hub, TopicCounts};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub sessions: Arc<SessionConfig>,
    pub max_clients: usize,
    next_session: Arc<AtomicU64>,
    started: Instant,
}

impl AppState {
    pub fn new(hub: Arc<Hub>, sessions: SessionConfig, max_clients: usize) -> Self {
        Self {
            hub,
            sessions: Arc::new(sessions),
            max_clients,
            next_session: Arc::new(AtomicU64::new(1)),
            started: Instant::now(),
        }
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

async fn ws(State(st): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    if st.hub.metrics.clients.load(Ordering::Relaxed) >= st.max_clients as i64 {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many clients\n").into_response();
    }
    let id = st.next_session.fetch_add(1, Ordering::Relaxed);
    upgrade.max_message_size(64 * 1024).on_upgrade(move |socket| session::run(socket, st.hub, st.sessions, id))
}

async fn readyz(State(st): State<AppState>) -> Response {
    let up = st.hub.provider().stats().connections_up;
    if up > 0 && !st.hub.mids.is_empty() {
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

async fn stats_json(State(st): State<AppState>) -> Response {
    axum::Json(stats(&st)).into_response()
}

async fn metrics(State(st): State<AppState>) -> Response {
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
