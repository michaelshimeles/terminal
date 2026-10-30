//! Read-only calls to Hyperliquid's public `info` endpoint, under a weight self-limit that stays
//! below Hyperliquid's 1200 per minute per IP (RL in docs/architecture/server.md).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tr_core::Bar;

use crate::limit::WindowLimiter;
use crate::metrics::Metrics;

/// `candleSnapshot` serves at most this many of the most recent bars per interval.
pub const CANDLE_CAP: usize = 5000;

/// Weight Hyperliquid charges for a `candleSnapshot` returning `n` bars: 20, plus 1 per 60.
pub fn candle_weight(n: usize) -> u32 {
    20 + n.div_ceil(60) as u32
}

pub struct Info {
    http: reqwest::Client,
    url: String,
    limiter: WindowLimiter,
    metrics: Arc<Metrics>,
}

/// One perp from `metaAndAssetCtxs`.
#[derive(Debug, Clone, PartialEq)]
pub struct PerpCtx {
    pub name: String,
    pub prev_day: Option<f64>,
    pub delisted: bool,
}

#[derive(Deserialize)]
struct Universe {
    universe: Vec<UniverseEntry>,
}

#[derive(Deserialize)]
struct UniverseEntry {
    name: String,
    #[serde(default, rename = "isDelisted")]
    delisted: bool,
}

#[derive(Deserialize)]
struct Ctx {
    #[serde(rename = "prevDayPx")]
    prev_day: Option<String>,
}

#[derive(Deserialize)]
struct Candle {
    t: i64,
    o: String,
    h: String,
    l: String,
    c: String,
    v: String,
}

impl Info {
    pub fn new(url: &str, weight_per_min: u32, metrics: Arc<Metrics>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("terminal-gateway/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self { http, url: url.into(), limiter: WindowLimiter::per_minute(weight_per_min), metrics })
    }

    /// Weight spent in the last minute, and the self-limit.
    pub fn weight(&self) -> (u32, u32) {
        (self.limiter.used(), self.limiter.budget())
    }

    async fn post<T: DeserializeOwned>(&self, body: Value, weight: u32) -> Result<T> {
        self.limiter.acquire(weight).await;
        Metrics::add(&self.metrics.rest_requests, 1);
        let kind = body["type"].as_str().unwrap_or("?").to_string();
        let result = async {
            let resp = self.http.post(&self.url).json(&body).send().await?;
            let status = resp.status();
            if status.as_u16() == 429 {
                // Keep every caller off the endpoint for a full window.
                self.limiter.charge(self.limiter.budget());
                bail!("rate limited (HTTP 429)");
            }
            if !status.is_success() {
                bail!("HTTP {status}");
            }
            Ok(resp.json::<T>().await?)
        }
        .await;
        if result.is_err() {
            Metrics::add(&self.metrics.rest_errors, 1);
        }
        result.with_context(|| format!("info {kind}"))
    }

    /// Perp dex names; `""` is the core dex.
    pub async fn perp_dexs(&self) -> Result<Vec<String>> {
        let v: Vec<Option<Value>> = self.post(json!({ "type": "perpDexs" }), 20).await?;
        Ok(v.into_iter().map(|d| d.and_then(|d| d["name"].as_str().map(str::to_string)).unwrap_or_default()).collect())
    }

    pub async fn perps(&self, dex: &str) -> Result<Vec<PerpCtx>> {
        let body = if dex.is_empty() {
            json!({ "type": "metaAndAssetCtxs" })
        } else {
            json!({ "type": "metaAndAssetCtxs", "dex": dex })
        };
        let (meta, ctxs): (Universe, Vec<Ctx>) = self.post(body, 20).await?;
        Ok(meta
            .universe
            .into_iter()
            .zip(ctxs)
            .map(|(u, c)| PerpCtx {
                name: u.name,
                prev_day: c.prev_day.and_then(|p| p.parse().ok()),
                delisted: u.delisted,
            })
            .collect())
    }

    /// Bars opening in `[start_ms, end_ms]`, oldest first. `expect` sizes the weight up front;
    /// any extra weight the response turns out to cost is charged afterwards.
    pub async fn candles(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
        expect: usize,
    ) -> Result<Vec<Bar>> {
        let body = json!({
            "type": "candleSnapshot",
            "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms },
        });
        let charged = candle_weight(expect.min(CANDLE_CAP));
        let rows: Vec<Candle> = self.post(body, charged).await?;
        let actual = candle_weight(rows.len());
        if actual > charged {
            self.limiter.charge(actual - charged);
        }
        let p = |s: &str| s.parse::<f64>().with_context(|| format!("candle number {s:?}"));
        let mut bars = rows
            .iter()
            .map(|r| Ok(Bar::new(r.t.div_euclid(1000), p(&r.o)?, p(&r.h)?, p(&r.l)?, p(&r.c)?, p(&r.v)?)))
            .collect::<Result<Vec<_>>>()?;
        bars.sort_by_key(|b| b.ts);
        bars.dedup_by_key(|b| b.ts);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candle_weights() {
        assert_eq!(candle_weight(0), 20);
        assert_eq!(candle_weight(60), 21);
        assert_eq!(candle_weight(61), 22);
        assert_eq!(candle_weight(CANDLE_CAP), 104);
    }
}
