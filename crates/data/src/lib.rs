//! Market data behind a pluggable [`MarketData`] trait, addressed by Bloomberg-style
//! market keys (`AAPL US Equity`, `BTCUSD Curncy`, `US10Y Govt`, `CL1 Comdty`, `SPX Index`).
//!
//! Provider selection ([`from_env`]):
//! - `TR_PROVIDER=sample` forces deterministic sample data (fully offline, with simulated
//!   live ticks).
//! - Otherwise [`markets::Markets`] routes each asset class to a free source: Binance public
//!   REST/websocket (Coinbase as fallback) for crypto, Treasury.gov (FRED fallback) for US
//!   Treasury yields, Yahoo Finance for FX, indices, futures, ETFs and stocks, or Alpaca for
//!   US stocks/ETFs when `APCA_API_KEY_ID` + `APCA_API_SECRET_KEY` are set.
//!
//! The router is wrapped in [`Fallback`], which serves sample data if a source is
//! unreachable, so the terminal always has something to draw.
//!
//! `TR_SERVER_URL` (`ws://` or `wss://`) opts in to the Terminal data server
//! ([`remote::RemoteMarkets`]): crypto markets then come from Hyperliquid perps through the
//! server, and everything else, or anything the server can't serve, from the sources above.
//!
//! The browser build (`wasm32`) can only reach sources that allow cross-origin requests:
//! [`for_browser`] serves live crypto through [`web::WebMarkets`] and sample data for the
//! rest. It has no SQLite bar cache.

#[cfg(not(target_arch = "wasm32"))]
pub mod alpaca;
#[cfg(not(target_arch = "wasm32"))]
pub mod cache;
pub mod crypto;
#[cfg(not(target_arch = "wasm32"))]
pub mod markets;
#[cfg(not(target_arch = "wasm32"))]
pub mod remote;
pub mod rss;
pub mod rt;
pub mod sample;
#[cfg(not(target_arch = "wasm32"))]
pub mod treasury;
pub mod web;
mod ws;
#[cfg(not(target_arch = "wasm32"))]
pub mod yahoo;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tr_core::{AssetClass, Bar, Security, Timeframe, catalog};

#[cfg(not(target_arch = "wasm32"))]
pub use cache::BarCache;

/// Latest quote for a symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub symbol: String,
    pub last: f64,
    pub prev_close: f64,
    pub ts: i64,
    /// Recent daily closes (oldest first) for sparklines; may be empty.
    #[serde(default)]
    pub spark: Vec<f64>,
}

/// A symbol-search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub key: String,
    pub name: String,
    pub class: AssetClass,
    pub exchange: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub ticker: String,
    pub tenor_years: f64,
    pub yield_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YieldCurve {
    /// Observation date (Unix seconds, midnight UTC).
    pub ts: i64,
    pub points: Vec<CurvePoint>,
}

impl YieldCurve {
    pub fn get(&self, ticker: &str) -> Option<f64> {
        self.points.iter().find(|p| p.ticker == ticker).map(|p| p.yield_pct)
    }
}

/// The US Treasury curve now, about a month ago and about a year ago.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YieldCurves {
    pub current: YieldCurve,
    pub month_ago: Option<YieldCurve>,
    pub year_ago: Option<YieldCurve>,
}

/// A news headline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsItem {
    pub title: String,
    pub publisher: String,
    pub link: String,
    /// Publication time (Unix seconds).
    pub ts: i64,
    /// Related tickers as reported by the source.
    #[serde(default)]
    pub tickers: Vec<String>,
}

/// One side's price level: (price, size).
pub type Level = (f64, f64);

/// Top of the order book: bids descending, asks ascending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBook {
    pub symbol: String,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    /// Venue the book came from (e.g. "Binance").
    pub source: String,
    /// True when the book is synthetic (sample mode or no public book for this market).
    pub simulated: bool,
}

impl OrderBook {
    pub fn mid(&self) -> Option<f64> {
        Some((self.bids.first()?.0 + self.asks.first()?.0) / 2.0)
    }

    pub fn spread(&self) -> Option<f64> {
        Some(self.asks.first()?.0 - self.bids.first()?.0)
    }

    /// Bucket levels to multiples of `tick` (bids round down, asks up), summing sizes.
    pub fn grouped(&self, tick: f64) -> OrderBook {
        if tick <= 0.0 {
            return self.clone();
        }
        let group = |levels: &[Level], up: bool| {
            let mut out: Vec<Level> = Vec::new();
            for &(p, q) in levels {
                let b = if up { (p / tick).ceil() } else { (p / tick).floor() } * tick;
                match out.last_mut() {
                    Some(last) if (last.0 - b).abs() < tick * 1e-6 => last.1 += q,
                    _ => out.push((b, q)),
                }
            }
            out
        };
        OrderBook { bids: group(&self.bids, false), asks: group(&self.asks, true), ..self.clone() }
    }
}

/// One listed option.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionContract {
    pub contract: String,
    pub strike: f64,
    pub last: Option<f64>,
    pub change: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub volume: Option<u64>,
    pub open_interest: Option<u64>,
    /// Annualised implied volatility (0.25 = 25%).
    pub iv: Option<f64>,
    pub in_the_money: bool,
}

impl OptionContract {
    pub fn mid(&self) -> Option<f64> {
        match (self.bid, self.ask) {
            (Some(b), Some(a)) if b > 0.0 && a > 0.0 => Some((a + b) / 2.0),
            _ => self.last,
        }
    }
}

/// Calls and puts for one expiration, plus the other listed expirations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChain {
    pub underlying: String,
    pub spot: f64,
    /// Continuous dividend yield used for greeks.
    pub dividend_yield: f64,
    /// All listed expirations (Unix seconds, ascending).
    pub expirations: Vec<i64>,
    /// The expiration shown.
    pub expiration: i64,
    pub calls: Vec<OptionContract>,
    pub puts: Vec<OptionContract>,
    /// Quote time (Unix seconds).
    pub ts: i64,
    pub source: String,
    /// Synthetic chain (offline sample mode or no source).
    pub simulated: bool,
}

/// Live bar updates for one symbol and timeframe: each message is the latest state of
/// the current (possibly still forming) bar. The feed stops when the receiver is dropped.
pub type LiveFeed = mpsc::Receiver<Bar>;

/// Pick curve snapshots from per-date rows: latest, nearest at or before -30d and -365d.
pub fn pick_curves(mut rows: Vec<YieldCurve>) -> Option<YieldCurves> {
    rows.retain(|r| !r.points.is_empty());
    rows.sort_by_key(|r| r.ts);
    let current = rows.last()?.clone();
    let at = |days: i64| rows.iter().rev().find(|r| r.ts <= current.ts - days * 86_400).cloned();
    Some(YieldCurves { month_ago: at(30), year_ago: at(365), current })
}

impl Quote {
    pub fn change(&self) -> f64 {
        self.last - self.prev_close
    }

    pub fn change_pct(&self) -> f64 {
        if self.prev_close == 0.0 { 0.0 } else { self.change() / self.prev_close * 100.0 }
    }
}

/// A source of historical bars and quotes.
#[async_trait]
pub trait MarketData: Send + Sync {
    /// Short provider name shown in the status bar.
    fn name(&self) -> &str;

    /// Up to `limit` bars at `tf` ending at or before `end` (Unix seconds; `None` = now),
    /// ascending by time. Paging older history means calling again with `end` set to the
    /// oldest bar already held.
    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>>;

    /// Latest quotes for `symbols`. The default derives them from daily history.
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let mut out = Vec::with_capacity(symbols.len());
        for s in symbols {
            let bars = self.history(s, Timeframe::D1, None, 2).await?;
            if let Some(last) = bars.last() {
                let prev = if bars.len() > 1 { bars[bars.len() - 2].close } else { last.open };
                out.push(Quote { symbol: s.clone(), last: last.close, prev_close: prev, ts: last.ts, spark: vec![] });
            }
        }
        Ok(out)
    }

    /// Stream live updates of the current bar, if the source supports it.
    fn live(&self, _symbol: &str, _tf: Timeframe) -> Option<LiveFeed> {
        None
    }

    /// Search securities beyond the built-in catalog (network sources).
    async fn search(&self, _query: &str) -> Result<Vec<SearchHit>> {
        Ok(Vec::new())
    }

    /// Recent headlines for a market key, or general market news for `None`.
    async fn news(&self, _symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        Ok(Vec::new())
    }

    /// Option chain for one expiration (`None` = nearest).
    async fn options(&self, symbol: &str, _expiration: Option<i64>) -> Result<OptionChain> {
        anyhow::bail!("no options data for {symbol}")
    }

    /// Top `depth` levels of the order book, where a public book exists.
    async fn order_book(&self, symbol: &str, _depth: usize) -> Result<OrderBook> {
        anyhow::bail!("no public order book for {symbol}")
    }

    /// The US Treasury curve. The default assembles it from each tenor's daily history.
    async fn yield_curves(&self) -> Result<YieldCurves> {
        let tenors = catalog::treasury_curve();
        let mut by_tenor = Vec::new();
        for l in &tenors {
            by_tenor.push((l, self.history(&l.key(), Timeframe::D1, None, 400).await?));
        }
        let mut dates: Vec<i64> = by_tenor.iter().flat_map(|(_, b)| b.iter().map(|x| x.ts)).collect();
        dates.sort_unstable();
        dates.dedup();
        let rows = dates
            .into_iter()
            .map(|ts| YieldCurve {
                ts,
                points: by_tenor
                    .iter()
                    .filter_map(|(l, bars)| {
                        let b = bars.iter().rev().find(|b| b.ts <= ts)?;
                        Some(CurvePoint { ticker: l.ticker.into(), tenor_years: l.tenor_years?, yield_pct: b.close })
                    })
                    .collect(),
            })
            .collect();
        pick_curves(rows).ok_or_else(|| anyhow::anyhow!("no Treasury curve data"))
    }
}

pub type Provider = Arc<dyn MarketData>;

/// Serves from `primary`, falling back to sample data on error.
pub struct Fallback<P> {
    primary: P,
    sample: sample::SampleProvider,
    name: String,
}

impl<P: MarketData> Fallback<P> {
    pub fn new(primary: P) -> Self {
        let name = primary.name().to_string();
        Self { primary, sample: sample::SampleProvider::default(), name }
    }
}

#[async_trait]
impl<P: MarketData> MarketData for Fallback<P> {
    fn name(&self) -> &str {
        &self.name
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        match self.primary.history(symbol, tf, end, limit).await {
            Ok(b) => Ok(b),
            // Paging older history: never splice sample bars onto real ones.
            Err(e) if end.is_some() => Err(e),
            Err(e) => {
                tracing::warn!("{} history {symbol} {tf} failed, using sample data: {e:#}", self.name);
                self.sample.history(symbol, tf, end, limit).await
            }
        }
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        match self.primary.quotes(symbols).await {
            // If the primary produced nothing at all (offline), serve sample quotes rather
            // than an empty panel; partial results are never mixed with synthetic prices.
            Ok(mut q) => {
                let missing: Vec<String> =
                    symbols.iter().filter(|s| !q.iter().any(|x| &x.symbol == *s)).cloned().collect();
                if !missing.is_empty() && q.is_empty() {
                    q.extend(self.sample.quotes(&missing).await.unwrap_or_default());
                }
                Ok(q)
            }
            Err(e) => {
                tracing::warn!("{} quotes failed, using sample data: {e:#}", self.name);
                self.sample.quotes(symbols).await
            }
        }
    }

    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        self.primary.live(symbol, tf)
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.primary.search(query).await
    }

    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        self.primary.news(symbol).await
    }

    async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<OptionChain> {
        match self.primary.options(symbol, expiration).await {
            Ok(c) => Ok(c),
            Err(e) => {
                tracing::warn!("{} options {symbol}: {e:#}; simulating", self.name);
                self.sample.options(symbol, expiration).await
            }
        }
    }

    /// Real books where the venue publishes one; otherwise a clearly flagged simulation.
    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        match self.primary.order_book(symbol, depth).await {
            Ok(b) => Ok(b),
            Err(e) => {
                tracing::debug!("{} order book {symbol}: {e:#}; simulating", self.name);
                self.sample.order_book(symbol, depth).await
            }
        }
    }

    async fn yield_curves(&self) -> Result<YieldCurves> {
        match self.primary.yield_curves().await {
            Ok(c) => Ok(c),
            Err(e) => {
                tracing::warn!("{} yield curve failed, using sample data: {e:#}", self.name);
                self.sample.yield_curves().await
            }
        }
    }
}

/// Resolve a market key or bare ticker.
pub fn security(symbol: &str) -> Security {
    Security::parse(symbol).unwrap_or_else(|| Security::equity(symbol))
}

/// Build the provider described in the crate docs from environment variables.
#[cfg(not(target_arch = "wasm32"))]
pub fn from_env() -> Provider {
    // reqwest and the websocket client both use rustls; pin one crypto provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if var("TR_PROVIDER").is_some_and(|p| p.eq_ignore_ascii_case("sample")) {
        return Arc::new(sample::SampleProvider::default().with_live_ticks());
    }
    let alpaca = match (var("APCA_API_KEY_ID"), var("APCA_API_SECRET_KEY")) {
        (Some(key), Some(secret)) => {
            let feed = var("APCA_DATA_FEED").unwrap_or_else(|| "iex".into());
            alpaca::AlpacaProvider::new(key, secret, feed).map_err(|e| tracing::warn!("alpaca init failed: {e:#}")).ok()
        }
        _ => None,
    };
    let markets = match markets::Markets::new(alpaca) {
        Ok(m) => Fallback::new(m),
        Err(_) => return Arc::new(sample::SampleProvider::default().with_live_ticks()),
    };
    match var("TR_SERVER_URL") {
        Some(url) => {
            // The URL itself stays out of the logs.
            tracing::info!("TR_SERVER_URL is set: crypto markets come through the Terminal server");
            Arc::new(remote::RemoteMarkets::new(url.trim(), markets))
        }
        None => Arc::new(markets),
    }
}

/// The browser build's provider: sample data only when `offline`, otherwise live crypto
/// with sample data for everything else.
pub fn for_browser(offline: bool) -> Provider {
    if !offline {
        match web::WebMarkets::new() {
            Ok(m) => return Arc::new(Fallback::new(m)),
            Err(e) => tracing::warn!("crypto sources unavailable, using sample data: {e:#}"),
        }
    }
    Arc::new(sample::SampleProvider::default().with_live_ticks())
}

/// Keep at most `limit` bars ending at or before `end`.
pub(crate) fn clip(mut bars: Vec<Bar>, end: Option<i64>, limit: usize) -> Vec<Bar> {
    if let Some(end) = end {
        bars.retain(|b| b.ts < end);
    }
    if bars.len() > limit {
        bars.drain(..bars.len() - limit);
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_and_simulated_books() {
        let b = OrderBook {
            symbol: "X".into(),
            bids: vec![(100.4, 1.0), (100.1, 2.0), (99.7, 1.0)],
            asks: vec![(100.6, 1.0), (100.9, 3.0), (101.2, 1.0)],
            source: "t".into(),
            simulated: false,
        };
        let g = b.grouped(0.5);
        assert_eq!(g.bids, vec![(100.0, 3.0), (99.5, 1.0)]);
        assert_eq!(g.asks, vec![(101.0, 4.0), (101.5, 1.0)]);
        assert!((b.mid().unwrap() - 100.5).abs() < 1e-9);
        let s = sample::simulated_book("BTCUSD Curncy", 65_000.0, 20, 7);
        assert!(s.simulated && s.bids.len() > 10 && s.asks.len() > 10);
        assert!(s.bids[0].0 < 65_000.0 && s.asks[0].0 > 65_000.0);
        assert!(s.bids.windows(2).all(|w| w[0].0 > w[1].0));
        assert_eq!(s, sample::simulated_book("BTCUSD Curncy", 65_000.0, 20, 7));
        // Simulated option chains: sorted strikes around spot, sane prices, parity-ish.
        let now = 1_791_800_000;
        let c = sample::simulated_chain("AAPL US Equity", 230.0, None, now);
        assert!(c.simulated && c.expirations.len() >= 10 && c.expiration > now);
        assert!(c.expirations.windows(2).all(|w| w[0] < w[1]));
        assert!(c.calls.windows(2).all(|w| w[0].strike < w[1].strike));
        assert!(c.calls.iter().any(|k| k.strike < 230.0) && c.calls.iter().any(|k| k.strike > 230.0));
        let deep = c.calls.first().unwrap();
        assert!(deep.in_the_money && deep.mid().unwrap() > 230.0 - deep.strike - 1.0);
        let later = sample::simulated_chain("AAPL US Equity", 230.0, Some(c.expirations[5]), now);
        assert_eq!(later.expiration, c.expirations[5]);
    }

    struct Broken;

    #[async_trait]
    impl MarketData for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        async fn history(&self, _: &str, _: Timeframe, _: Option<i64>, _: usize) -> Result<Vec<Bar>> {
            anyhow::bail!("offline")
        }
    }

    #[tokio::test]
    async fn fallback_serves_sample_data() {
        let p = Fallback::new(Broken);
        let bars = p.history("AAPL", Timeframe::D1, None, 100).await.unwrap();
        assert_eq!(bars.len(), 100);
        let q = p.quotes(&["AAPL".into()]).await.unwrap();
        assert_eq!(q.len(), 1);
        // Paging must surface the error instead of mixing in synthetic bars.
        assert!(p.history("AAPL", Timeframe::D1, Some(bars[0].ts), 100).await.is_err());
        assert_eq!(p.name(), "broken");
    }

    #[tokio::test]
    async fn default_yield_curve_from_history() {
        let c = sample::SampleProvider::at(1_790_000_000, 1_000).yield_curves().await.unwrap();
        assert_eq!(c.current.points.len(), 11);
        assert!(c.month_ago.is_some() && c.year_ago.is_some());
        assert!(c.current.get("US10Y").is_some_and(|y| y > 0.0 && y < 20.0));
    }

    #[test]
    fn curve_picking() {
        let row = |d: i64, y: f64| YieldCurve {
            ts: d * 86_400,
            points: vec![CurvePoint { ticker: "US10Y".into(), tenor_years: 10.0, yield_pct: y }],
        };
        let c = pick_curves(vec![row(400, 1.0), row(1, 9.0), row(370, 2.0), row(760, 3.0), row(800, 4.0)]).unwrap();
        assert_eq!(c.current.get("US10Y"), Some(4.0));
        assert_eq!(c.month_ago.unwrap().get("US10Y"), Some(3.0));
        assert_eq!(c.year_ago.unwrap().get("US10Y"), Some(1.0)); // latest at or before day 435
    }

    #[test]
    fn clip_respects_end_and_limit() {
        let bars: Vec<Bar> = (0..10).map(|i| Bar::new(i, 1.0, 1.0, 1.0, 1.0, 1.0)).collect();
        let c = clip(bars, Some(8), 3);
        assert_eq!(c.iter().map(|b| b.ts).collect::<Vec<_>>(), vec![5, 6, 7]);
    }

    #[test]
    fn quote_change() {
        let q = Quote { symbol: "X".into(), last: 110.0, prev_close: 100.0, ts: 0, spark: vec![] };
        assert_eq!(q.change(), 10.0);
        assert!((q.change_pct() - 10.0).abs() < 1e-9);
    }
}
