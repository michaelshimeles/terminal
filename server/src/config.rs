//! Gateway settings from environment variables. See `server/README.md` for the list.

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::hl::HlConfig;
use crate::session::SessionConfig;
use crate::store::HubConfig;

#[derive(Debug, Clone)]
pub struct Config {
    /// Plain HTTP and websocket. TLS ends at the edge in front of the gateway.
    pub bind: SocketAddr,
    pub hl: HlConfig,
    pub hub: HubConfig,
    pub session: SessionConfig,
    pub max_clients: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 8787)),
            hl: HlConfig::default(),
            hub: HubConfig::default(),
            session: SessionConfig::default(),
            max_clients: 1000,
        }
    }
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn num<T: std::str::FromStr>(k: &str) -> Result<Option<T>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    var(k).map(|v| v.parse::<T>().with_context(|| format!("{k}={v}"))).transpose()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut c = Config::default();
        if let Some(b) = var("TR_GATEWAY_BIND") {
            c.bind = b
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .with_context(|| format!("TR_GATEWAY_BIND={b} (want host:port)"))?;
        }
        if let Some(u) = var("TR_HL_WS_URL") {
            c.hl.pool.ws_url = u;
        }
        if let Some(u) = var("TR_HL_INFO_URL") {
            c.hl.info_url = u;
        }
        if let Some(d) = var("TR_HL_DEXES").filter(|d| !d.eq_ignore_ascii_case("all")) {
            c.hl.dexes = Some(d.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
        }
        if let Some(n) = num("TR_UPSTREAM_CONNECTIONS")? {
            c.hl.pool.max_connections = n;
        }
        if let Some(n) = num("TR_UPSTREAM_SUBSCRIPTIONS")? {
            c.hl.pool.max_subscriptions = n;
        }
        if let Some(n) = num("TR_UPSTREAM_MESSAGES_PER_MIN")? {
            c.hl.pool.messages_per_min = n;
        }
        if let Some(n) = num("TR_REST_WEIGHT_PER_MIN")? {
            c.hl.rest_weight_per_min = n;
        }
        if let Some(s) = num::<f64>("TR_LINGER_SECS")? {
            c.hub.linger = Duration::from_secs_f64(s.max(0.0));
        }
        if let Some(n) = num("TR_MAX_CLIENTS")? {
            c.max_clients = n;
        }
        Ok(c)
    }
}

pub const HELP: &str = "\
tr-gateway: the Terminal data server (interim: Hyperliquid's public feed)

Serves websocket clients at /ws, plus /healthz, /readyz, /metrics and /stats.
Plain HTTP only: put a TLS-terminating edge in front of it for wss:// on 443.

Environment:
  TR_GATEWAY_BIND               listen address, host:port (default localhost:8787)
  TR_HL_DEXES                   HIP-3 dexes besides the core dex: all (default) or a list like xyz,flx
  TR_UPSTREAM_CONNECTIONS       upstream websocket connections, 2 to 8 (default 3)
  TR_UPSTREAM_SUBSCRIPTIONS     upstream subscriptions, at most 950 (default 900)
  TR_UPSTREAM_MESSAGES_PER_MIN  messages sent upstream a minute, at most 1800 (default 1000)
  TR_REST_WEIGHT_PER_MIN        info endpoint weight a minute, at most 1100 (default 800)
  TR_LINGER_SECS                keep an unwatched market's streams this long (default 20)
  TR_MAX_CLIENTS                client connections (default 1000)
  TR_HL_WS_URL, TR_HL_INFO_URL  upstream endpoints (default Hyperliquid mainnet)
  RUST_LOG                      log filter (default info)
";
