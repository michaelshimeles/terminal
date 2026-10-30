//! `HlPublicProvider`: Hyperliquid through its public API, the interim source until our own node
//! is up (docs/architecture/server.md, "Interim: public feed"). Read-only public calls only: no
//! `exchange` endpoint, no user-specific subscriptions, no orders. A node-backed provider
//! replaces it behind the same [`Provider`] trait.

pub mod pool;
pub mod rest;
pub mod wire;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use tr_core::{Bar, Timeframe};
use tr_proto::InstrumentKind;

use crate::metrics::Metrics;
use crate::provider::{
    Capabilities, Channel, ChannelKind, DisplayPolicy, InstrumentDef, InstrumentId, Provider, ProviderId,
    SubscribeError, UpstreamHandle, UpstreamSink, UpstreamStats,
};
use pool::{Pool, PoolConfig};
use rest::{CANDLE_CAP, Info};
use wire::SubKey;

pub const MAINNET_WS: &str = "wss://api.hyperliquid.xyz/ws";
pub const MAINNET_INFO: &str = "https://api.hyperliquid.xyz/info";

#[derive(Debug, Clone)]
pub struct HlConfig {
    pub info_url: String,
    pub pool: PoolConfig,
    /// HIP-3 dexes to carry besides the core dex; `None` means every dex `perpDexs` lists.
    pub dexes: Option<Vec<String>>,
    /// REST weight per minute. Capped at 1100, below Hyperliquid's 1200 per IP.
    pub rest_weight_per_min: u32,
}

impl Default for HlConfig {
    fn default() -> Self {
        Self { info_url: MAINNET_INFO.into(), pool: PoolConfig::new(MAINNET_WS), dexes: None, rest_weight_per_min: 800 }
    }
}

pub struct HlPublicProvider {
    pool: Pool,
    info: Info,
    dexes: Option<Vec<String>>,
}

impl HlPublicProvider {
    pub fn new(cfg: HlConfig, metrics: Arc<Metrics>) -> Result<Self> {
        Ok(Self {
            pool: Pool::new(cfg.pool, metrics.clone()),
            info: Info::new(&cfg.info_url, cfg.rest_weight_per_min.clamp(20, 1100), metrics)?,
            dexes: cfg.dexes,
        })
    }
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[async_trait]
impl Provider for HlPublicProvider {
    fn id(&self) -> ProviderId {
        ProviderId("hyperliquid-public")
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            channels: vec![ChannelKind::Mids, ChannelKind::Book, ChannelKind::Trades, ChannelKind::Bars],
            kinds: vec![InstrumentKind::Perp, InstrumentKind::Spot],
        }
    }

    fn display_policy(&self) -> DisplayPolicy {
        DisplayPolicy { public_display: true, entitlement: None, delay: None, attribution: "Data: Hyperliquid" }
    }

    fn close(&self) {
        self.pool.close();
    }

    fn stats(&self) -> UpstreamStats {
        let (used, budget) = self.info.weight();
        UpstreamStats { rest_weight_last_min: used, rest_weight_per_min: budget, ..self.pool.stats() }
    }

    /// Perps on the core dex and the selected HIP-3 dexes, with their previous-day prices. Spot
    /// markets aren't listed here, but they stream: the core dex's mids carry them.
    async fn instruments(&self) -> Result<Vec<InstrumentDef>> {
        let mut dexes = self.info.perp_dexs().await?;
        if let Some(want) = &self.dexes {
            dexes.retain(|d| d.is_empty() || want.contains(d));
        }
        let mut out = Vec::new();
        for dex in dexes {
            match self.info.perps(&dex).await {
                Ok(perps) => out.extend(perps.into_iter().filter(|p| !p.delisted).map(|p| InstrumentDef {
                    id: InstrumentId(p.name),
                    venue: dex.clone(),
                    kind: InstrumentKind::Perp,
                    prev_day: p.prev_day,
                })),
                // One dex failing shouldn't hide the others.
                Err(e) => tracing::warn!("dex {dex:?}: {e:#}"),
            }
        }
        Ok(out)
    }

    /// From `candleSnapshot`, which only serves the most recent 5000 bars per interval: paging
    /// further back returns nothing (the interim feed's history cap, until node-built candles).
    async fn history(&self, inst: &InstrumentId, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let limit = limit.clamp(1, CANDLE_CAP);
        let end_s = end.unwrap_or_else(|| now_s() + 1);
        // Months are counted as 31 days so the window always covers `limit` bars.
        let span = if tf == Timeframe::MN1 { 31 * 86_400 } else { tf.seconds() };
        let start_s = end_s - span * (limit as i64 + 1);
        let bars =
            self.info.candles(inst.as_str(), wire::interval(tf), start_s * 1000, end_s * 1000 - 1, limit).await?;
        Ok(crate::clip(bars, end, limit))
    }

    async fn subscribe(
        &self,
        ch: Channel,
        inst: &InstrumentId,
        sink: UpstreamSink,
    ) -> Result<UpstreamHandle, SubscribeError> {
        let coin = inst.0.clone();
        let key = match ch {
            Channel::Mids => SubKey::AllMids { dex: coin },
            Channel::Book => SubKey::L2Book { coin },
            Channel::Trades => SubKey::Trades { coin },
            Channel::Bars(tf) => SubKey::Candle { coin, interval: wire::interval(tf) },
        };
        let guard = self.pool.subscribe(key, sink, ch == Channel::Mids)?;
        Ok(UpstreamHandle::new(guard))
    }
}
