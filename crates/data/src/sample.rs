use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tr_core::{Bar, Timeframe, sample};

use crate::{LiveFeed, MarketData, OptionChain, OrderBook, rt, security};

/// Deterministic synthetic data anchored at a fixed "now" per process, so paging is
/// consistent: page N+1 is exactly the bars preceding page N. Levels and volatility
/// follow the asset class (BTCUSD near 65,000, EURUSD near 1.09, US10Y near 4.2%).
#[derive(Debug, Clone)]
pub struct SampleProvider {
    now: i64,
    /// Total history available per symbol/timeframe.
    pub depth: usize,
    live_ticks: bool,
}

impl Default for SampleProvider {
    /// History depth defaults to 20,000 bars; `TR_SAMPLE_BARS` raises it (e.g. 1000000).
    fn default() -> Self {
        let depth = std::env::var("TR_SAMPLE_BARS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
        Self { now: chrono::Utc::now().timestamp(), depth, live_ticks: false }
    }
}

impl SampleProvider {
    pub fn at(now: i64, depth: usize) -> Self {
        Self { now, depth, live_ticks: false }
    }

    /// Also simulate a live tick stream (offline mode).
    pub fn with_live_ticks(mut self) -> Self {
        self.live_ticks = true;
        self
    }

    fn bars(&self, symbol: &str, tf: Timeframe) -> Vec<Bar> {
        sample::for_security(&security(symbol), tf, self.depth, self.now)
    }
}

#[async_trait]
impl MarketData for SampleProvider {
    fn name(&self) -> &str {
        "sample"
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        Ok(crate::clip(self.bars(symbol, tf), end, limit))
    }

    /// A synthetic chain priced with Black-Scholes around the last price.
    async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<OptionChain> {
        let spot = self.bars(symbol, Timeframe::D1).last().map(|b| b.close).unwrap_or(100.0);
        Ok(simulated_chain(symbol, spot, expiration, chrono::Utc::now().timestamp()))
    }

    /// A synthetic book around the last price; sizes vary with the clock so it moves.
    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        let last = self.bars(symbol, Timeframe::M1).last().map(|b| b.close).unwrap_or(100.0);
        Ok(simulated_book(symbol, last, depth, chrono::Utc::now().timestamp_millis() as u64 / 500))
    }

    /// Random-walk ticks on the last bar every 400 ms, rolling into new bars on schedule.
    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        if !self.live_ticks {
            return None;
        }
        let mut bar = *self.bars(symbol, tf).last()?;
        let sec = security(symbol);
        let (tx, rx) = mpsc::channel(64);
        let tick_vol = sec.class.sample_vol() * (0.4 / (365.0 * 86_400.0f64)).sqrt() * 8.0;
        let mut rng = sample::Rng::new(sample::seed_for(symbol) ^ 0x5eed);
        rt::spawn(async move {
            let mut iv = rt::interval(Duration::from_millis(400));
            loop {
                iv.tick().await;
                let now = chrono::Utc::now().timestamp();
                let bucket = tf.bucket_start(now);
                if bucket > bar.ts {
                    bar = Bar::new(bucket, bar.close, bar.close, bar.close, bar.close, 0.0);
                }
                let px = bar.close * (tick_vol * rng.normal()).exp();
                bar.close = px;
                bar.high = bar.high.max(px);
                bar.low = bar.low.min(px);
                bar.volume += (rng.f64() * 100.0).round();
                if tx.send(bar).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

/// Deterministic synthetic option chain: weekly expirations for two months then monthly,
/// strikes around `spot`, prices from Black-Scholes with a volatility smile.
pub fn simulated_chain(symbol: &str, spot: f64, expiration: Option<i64>, now: i64) -> OptionChain {
    let day = 86_400;
    // Fridays 20:00 UTC (close), weekly for 8 weeks, then every 4 weeks.
    let next_friday = {
        let d = now.div_euclid(day);
        let dow = (d + 4).rem_euclid(7); // 0 = Sunday
        (d + (5 - dow).rem_euclid(7)) * day + 20 * 3600
    };
    let first = if next_friday <= now { next_friday + 7 * day } else { next_friday };
    let mut expirations: Vec<i64> = (0..8).map(|k| first + k * 7 * day).collect();
    expirations.extend((1..10).map(|k| first + (7 + 4 * k) * 7 * day));
    let exp = expiration.filter(|e| expirations.contains(e)).unwrap_or(expirations[0]);
    let t = ((exp - now) as f64 / (365.0 * day as f64)).max(1.0 / 365.0);
    let base_vol = security(symbol).class.sample_vol().clamp(0.12, 0.9);
    let step = {
        let raw = spot * 0.0125;
        let mag = 10f64.powf(raw.log10().floor());
        [1.0, 2.5, 5.0, 10.0].into_iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(raw)
    };
    let atm = (spot / step).round() * step;
    let mut rng = sample::Rng::new(sample::seed_for(symbol) ^ exp as u64);
    let (mut calls, mut puts) = (Vec::new(), Vec::new());
    for k in -20i32..=20 {
        let strike = atm + k as f64 * step;
        if strike <= 0.0 {
            continue;
        }
        let m = (strike / spot).ln();
        let vol = base_vol * (1.0 + 1.6 * m * m - 0.35 * m);
        for call in [true, false] {
            let Some((px, _)) = tr_core::options::black_scholes(call, spot, strike, t, 0.04, 0.0, vol) else {
                continue;
            };
            let px = px.max(0.01);
            let spread = (px * 0.03).max(0.01);
            let oi = (2_000.0 * (-(m * m) * 40.0).exp() * (0.5 + rng.f64())) as u64;
            let c = crate::OptionContract {
                contract: format!(
                    "{}{}{}{:08}",
                    security(symbol).ticker,
                    chrono::DateTime::from_timestamp(exp, 0)
                        .map(|d| d.format("%y%m%d").to_string())
                        .unwrap_or_default(),
                    if call { 'C' } else { 'P' },
                    (strike * 1000.0).round() as u64
                ),
                strike,
                last: Some((px * (1.0 + 0.02 * rng.normal())).max(0.01)),
                change: Some(px * 0.05 * rng.normal()),
                bid: Some((px - spread / 2.0).max(0.0)),
                ask: Some(px + spread / 2.0),
                volume: Some(oi / 5),
                open_interest: Some(oi),
                iv: Some(vol),
                in_the_money: if call { strike < spot } else { strike > spot },
            };
            if call { calls.push(c) } else { puts.push(c) }
        }
    }
    OptionChain {
        underlying: security(symbol).ticker,
        spot,
        dividend_yield: 0.0,
        expirations,
        expiration: exp,
        calls,
        puts,
        ts: now,
        source: "Simulated".into(),
        simulated: true,
    }
}

/// Deterministic synthetic order book for `symbol` around `mid` (varies with `step`).
pub fn simulated_book(symbol: &str, mid: f64, depth: usize, step: u64) -> OrderBook {
    let sec = security(symbol);
    let tick = 10f64.powi(-(sec.decimals(mid) as i32)).max(mid * 2e-5);
    let mut rng = sample::Rng::new(sample::seed_for(symbol) ^ step);
    let base = 10_000.0 / mid.max(1e-9);
    let mut side = |dir: f64| {
        (0..depth)
            .map(|i| {
                let px = mid + dir * tick * (i as f64 + 0.5 + rng.f64() * 0.5).round().max(1.0);
                let size = base * (0.2 + rng.f64() * 2.0) * (1.0 + i as f64 * 0.08);
                (px, size)
            })
            .collect::<Vec<_>>()
    };
    let mut bids = side(-1.0);
    let mut asks = side(1.0);
    bids.sort_by(|a, b| b.0.total_cmp(&a.0));
    asks.sort_by(|a, b| a.0.total_cmp(&b.0));
    bids.dedup_by(|a, b| a.0 == b.0);
    asks.dedup_by(|a, b| a.0 == b.0);
    OrderBook { symbol: symbol.to_string(), bids, asks, source: "Simulated".into(), simulated: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paging_is_contiguous() {
        let p = SampleProvider::at(1_790_000_000, 1_000);
        let recent = p.history("AAPL", Timeframe::H1, None, 300).await.unwrap();
        let older = p.history("AAPL", Timeframe::H1, Some(recent[0].ts), 300).await.unwrap();
        assert_eq!(older.len(), 300);
        assert_eq!(older.last().unwrap().ts + 3600, recent[0].ts);
        assert_eq!(older.last().unwrap().close, recent[0].open);
    }

    #[tokio::test]
    async fn class_aware_levels_and_quotes() {
        let p = SampleProvider::at(1_790_000_000, 500);
        let q = p.quotes(&["BTCUSD Curncy".into(), "US10Y Govt".into(), "EURUSD Curncy".into()]).await.unwrap();
        assert!(q[0].last > 10_000.0);
        assert!(q[1].last > 0.0 && q[1].last < 20.0);
        assert!(q[2].last > 0.5 && q[2].last < 2.0);
    }

    #[tokio::test]
    async fn simulated_ticks() {
        let p = SampleProvider::at(chrono::Utc::now().timestamp(), 100).with_live_ticks();
        let mut rx = p.live("ETHUSD Curncy", Timeframe::M1).unwrap();
        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        assert!(a.high >= a.low && b.ts >= a.ts && b.close > 0.0);
        assert!(SampleProvider::at(0, 10).live("X", Timeframe::M1).is_none());
    }
}
