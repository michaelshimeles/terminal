//! `tr-gateway`, the Terminal data server (`docs/architecture/server.md`).
//!
//! For now it carries Hyperliquid's public feed ([`hl::HlPublicProvider`]): one shared set of
//! upstream connections, fanned out to every client over the protocol in `tr-proto`.

pub mod config;
pub mod hl;
pub mod http;
pub mod limit;
pub mod metrics;
pub mod provider;
pub mod session;
pub mod store;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tr_core::Bar;

pub use config::Config;

use crate::hl::HlPublicProvider;
use crate::metrics::Metrics;
use crate::store::Hub;

/// Keep at most `limit` bars opening before `end`.
pub fn clip(mut bars: Vec<Bar>, end: Option<i64>, limit: usize) -> Vec<Bar> {
    if let Some(end) = end {
        bars.retain(|b| b.ts < end);
    }
    if bars.len() > limit {
        bars.drain(..bars.len() - limit);
    }
    bars
}

/// A running gateway.
pub struct Gateway {
    pub addr: SocketAddr,
    pub state: http::AppState,
    stop: Option<oneshot::Sender<()>>,
    closing: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Gateway {
    pub fn hub(&self) -> &Arc<Hub> {
        &self.state.hub
    }

    /// Stop accepting, close every client session and upstream connection, and wait for the
    /// server to finish. Nothing keeps running afterwards.
    pub async fn shutdown(mut self) {
        self.closing.send_replace(true);
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        self.state.hub.stop();
        let _ = (&mut self.task).await;
    }

    /// Run until the process is asked to stop.
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// Bind, connect upstream and serve in the background.
pub async fn start(cfg: Config) -> Result<Gateway> {
    let metrics = Arc::new(Metrics::default());
    let provider = Arc::new(HlPublicProvider::new(cfg.hl.clone(), metrics.clone())?);
    let hub = Hub::new(provider, cfg.hub.clone(), metrics);
    hub.start();
    let (closing, closing_rx) = watch::channel(false);
    let state = http::AppState::new(hub, cfg.session.clone(), cfg.max_clients, closing_rx);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await.context("binding the listen address")?;
    let addr = listener.local_addr()?;
    let (stop, stopped) = oneshot::channel::<()>();
    let app = http::router(state.clone());
    let task = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
            tracing::error!("server: {e}");
        }
    });
    Ok(Gateway { addr, state, stop: Some(stop), closing, task })
}
