//! Crypto market data from free public endpoints: Binance spot REST and websocket kline
//! streams (via the public market-data mirror, then the main API), with Coinbase Exchange
//! REST candles and its websocket ticker channel as fallback.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use serde_json::Value;
use tokio::sync::mpsc;
use tr_core::{Bar, Security, Timeframe, catalog};

use crate::ws::Socket;
use crate::{LiveFeed, Quote, rt};

const BINANCE_REST: [&str; 2] = ["https://data-api.binance.vision", "https://api.binance.com"];
const BINANCE_WS: [&str; 2] = ["wss://data-stream.binance.vision/ws", "wss://stream.binance.com:9443/ws"];
const COINBASE_REST: &str = "https://api.exchange.coinbase.com";
const COINBASE_WS: &str = "wss://ws-feed.exchange.coinbase.com";

/// Binance spot symbol for a crypto security (`BTCUSD` -> `BTCUSDT`).
pub fn binance_symbol(sec: &Security) -> String {
    if let Some(s) = catalog::by_security(sec).and_then(|l| l.binance) {
        return s.to_string();
    }
    match sec.ticker.strip_suffix("USD") {
        Some(base) => format!("{base}USDT"),
        None => sec.ticker.clone(),
    }
}

/// Coinbase product id (`BTCUSD` -> `BTC-USD`).
pub fn coinbase_product(sec: &Security) -> String {
    if let Some(s) = catalog::by_security(sec).and_then(|l| l.coinbase) {
        return s.to_string();
    }
    match sec.pair() {
        Some((base, quote)) => format!("{base}-{}", if quote == "USDT" { "USD" } else { quote }),
        None => sec.ticker.clone(),
    }
}

pub fn binance_interval(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::M1 => "1m",
        Timeframe::M5 => "5m",
        Timeframe::M15 => "15m",
        Timeframe::M30 => "30m",
        Timeframe::H1 => "1h",
        Timeframe::H4 => "4h",
        Timeframe::D1 => "1d",
        Timeframe::W1 => "1w",
        Timeframe::MN1 => "1M",
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Parse `/api/v3/klines`: arrays of `[open_ms, o, h, l, c, v, close_ms, ...]` with
/// prices as strings.
pub fn parse_binance_klines(json: &str) -> Result<Vec<Bar>> {
    let rows: Vec<Vec<Value>> = serde_json::from_str(json).context("parsing Binance klines")?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(Bar::new(
                r.first()?.as_i64()? / 1000,
                num(r.get(1)?)?,
                num(r.get(2)?)?,
                num(r.get(3)?)?,
                num(r.get(4)?)?,
                num(r.get(5)?)?,
            ))
        })
        .collect())
}

/// Parse `/api/v3/ticker/24hr`: `(symbol, last, open_24h)`.
pub fn parse_binance_24hr(json: &str) -> Result<Vec<(String, f64, f64)>> {
    let rows: Vec<Value> = serde_json::from_str(json).context("parsing Binance 24hr ticker")?;
    Ok(rows
        .iter()
        .filter_map(|r| Some((r["symbol"].as_str()?.to_string(), num(&r["lastPrice"])?, num(&r["openPrice"])?)))
        .collect())
}

/// Parse a websocket `kline` event into the current bar.
pub fn parse_binance_ws(text: &str) -> Option<Bar> {
    let v: Value = serde_json::from_str(text).ok()?;
    let k = v.get("k")?;
    Some(Bar::new(k["t"].as_i64()? / 1000, num(&k["o"])?, num(&k["h"])?, num(&k["l"])?, num(&k["c"])?, num(&k["v"])?))
}

/// Parse Coinbase `/products/{id}/candles`: `[time, low, high, open, close, volume]`,
/// newest first. Returned ascending.
pub fn parse_coinbase_candles(json: &str) -> Result<Vec<Bar>> {
    let rows: Vec<Vec<f64>> = serde_json::from_str(json).context("parsing Coinbase candles")?;
    let mut bars: Vec<Bar> =
        rows.iter().filter(|r| r.len() >= 6).map(|r| Bar::new(r[0] as i64, r[3], r[2], r[1], r[4], r[5])).collect();
    bars.sort_by_key(|b| b.ts);
    bars.dedup_by_key(|b| b.ts);
    Ok(bars)
}

/// Parse Coinbase `/products/{id}/stats`: `(last, open_24h)`.
pub fn parse_coinbase_stats(json: &str) -> Result<(f64, f64)> {
    let v: Value = serde_json::from_str(json).context("parsing Coinbase stats")?;
    Ok((num(&v["last"]).ok_or_else(|| anyhow!("no last"))?, num(&v["open"]).ok_or_else(|| anyhow!("no open"))?))
}

/// Parse a Coinbase websocket `ticker` message: `(unix_secs, price, size)`.
pub fn parse_coinbase_ws(text: &str) -> Option<(i64, f64, f64)> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v["type"] != "ticker" {
        return None;
    }
    let ts = DateTime::parse_from_rfc3339(v["time"].as_str()?).ok()?.timestamp();
    Some((ts, num(&v["price"])?, num(&v["last_size"]).unwrap_or(0.0)))
}

/// Coinbase granularity used to build `tf` bars, and whether resampling is needed.
fn coinbase_granularity(tf: Timeframe) -> (i64, bool) {
    match tf {
        Timeframe::M1 => (60, false),
        Timeframe::M5 => (300, false),
        Timeframe::M15 => (900, false),
        Timeframe::M30 => (900, true),
        Timeframe::H1 => (3600, false),
        Timeframe::H4 => (3600, true),
        Timeframe::D1 => (86_400, false),
        Timeframe::W1 | Timeframe::MN1 => (86_400, true),
    }
}

fn parse_levels(v: &serde_json::Value) -> Vec<crate::Level> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let p = l.get(0)?.as_str()?.parse().ok()?;
                    let q = l.get(1)?.as_str()?.parse().ok()?;
                    Some((p, q))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a Binance `/api/v3/depth` snapshot.
pub fn parse_binance_depth(json: &str, symbol: &str) -> Result<crate::OrderBook> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Binance depth")?;
    if v["bids"].is_null() {
        bail!("unexpected Binance depth payload");
    }
    Ok(crate::OrderBook {
        symbol: symbol.into(),
        bids: parse_levels(&v["bids"]),
        asks: parse_levels(&v["asks"]),
        source: "Binance".into(),
        simulated: false,
    })
}

/// Parse a Coinbase Exchange level-2 `/book` snapshot (`[price, size, orders]`).
pub fn parse_coinbase_book(json: &str, symbol: &str, depth: usize) -> Result<crate::OrderBook> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Coinbase book")?;
    if v["bids"].is_null() {
        bail!("unexpected Coinbase book payload");
    }
    let mut b = crate::OrderBook {
        symbol: symbol.into(),
        bids: parse_levels(&v["bids"]),
        asks: parse_levels(&v["asks"]),
        source: "Coinbase".into(),
        simulated: false,
    };
    b.bids.truncate(depth);
    b.asks.truncate(depth);
    Ok(b)
}

const TIMEOUT: Duration = Duration::from_secs(20);

pub struct CryptoProvider {
    client: reqwest::Client,
}

impl CryptoProvider {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder();
        // Browsers set their own User-Agent, and fetch takes its timeout per request.
        #[cfg(not(target_arch = "wasm32"))]
        let client = client.user_agent(concat!("terminal-rs/", env!("CARGO_PKG_VERSION"))).timeout(TIMEOUT);
        Ok(Self { client: client.build()? })
    }

    async fn get(&self, url: &str, query: &[(&str, String)]) -> Result<String> {
        let req = self.client.get(url).query(query);
        #[cfg(target_arch = "wasm32")]
        let req = req.timeout(TIMEOUT);
        let fetch = async move {
            let resp = req.send().await?;
            if !resp.status().is_success() {
                bail!("GET {url}: HTTP {}", resp.status());
            }
            Ok(resp.text().await?)
        };
        // A browser fetch holds JavaScript values, which are not `Send`. The web build has
        // one thread, so asserting it is sound and keeps the provider traits shared.
        #[cfg(target_arch = "wasm32")]
        let fetch = send_wrapper::SendWrapper::new(fetch);
        fetch.await
    }

    async fn binance_history(
        &self,
        host: &str,
        symbol: &str,
        tf: Timeframe,
        end: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Bar>> {
        let mut out: Vec<Bar> = Vec::new();
        let mut end_ms = end.map(|e| e * 1000 - 1);
        while out.len() < limit {
            let n = (limit - out.len()).min(1000);
            let mut q = vec![
                ("symbol", symbol.to_string()),
                ("interval", binance_interval(tf).into()),
                ("limit", n.to_string()),
            ];
            if let Some(e) = end_ms {
                q.push(("endTime", e.to_string()));
            }
            let page = parse_binance_klines(&self.get(&format!("{host}/api/v3/klines"), &q).await?)?;
            let Some(first) = page.first() else { break };
            end_ms = Some(first.ts * 1000 - 1);
            let got = page.len();
            out.splice(0..0, page);
            if got < n {
                break;
            }
        }
        Ok(out)
    }

    async fn coinbase_history(&self, product: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let (gran, resample) = coinbase_granularity(tf);
        let per_bar = (tf.seconds() / gran).max(1) as usize;
        let want = limit * per_bar;
        let mut end_ts = end.unwrap_or_else(|| chrono::Utc::now().timestamp());
        let mut out: Vec<Bar> = Vec::new();
        // Coinbase returns at most 300 candles per request; bound the walk.
        for _ in 0..40 {
            if out.len() >= want {
                break;
            }
            let start_ts = end_ts - 300 * gran;
            let fmt = |t: i64| DateTime::from_timestamp(t, 0).map(|d| d.to_rfc3339()).unwrap_or_default();
            let q = [("granularity", gran.to_string()), ("start", fmt(start_ts)), ("end", fmt(end_ts))];
            let page =
                parse_coinbase_candles(&self.get(&format!("{COINBASE_REST}/products/{product}/candles"), &q).await?)?;
            if page.is_empty() {
                break;
            }
            end_ts = page[0].ts - 1;
            let first = out.first().map(|f| f.ts);
            out.splice(0..0, page.into_iter().filter(|b| first.is_none_or(|f| b.ts < f)));
        }
        if let Some(e) = end {
            out.retain(|b| b.ts < e);
        }
        if resample {
            out = tf.resample(&out);
        }
        Ok(crate::clip(out, None, limit))
    }

    /// Crypto headlines from CoinDesk, narrowed to the asset when it is mentioned.
    pub async fn news(&self, sec: &Security) -> Result<Vec<crate::NewsItem>> {
        let items = crate::rss::parse_rss(&self.get(crate::rss::COINDESK, &[]).await?, "CoinDesk")?;
        let base = sec.ticker.trim_end_matches("USD").trim_end_matches("USDT");
        let name = sec.name().unwrap_or(base).split(['/', ' ']).next().unwrap_or(base).to_string();
        Ok(crate::rss::filter_relevant(items, &[base, &name]))
    }

    /// Order book snapshot: Binance first, Coinbase as fallback.
    pub async fn order_book(&self, key: &str, sec: &Security, depth: usize) -> Result<crate::OrderBook> {
        let limit = [5, 10, 20, 50, 100, 500].into_iter().find(|&l| l >= depth).unwrap_or(500);
        let mut last_err = None;
        for host in BINANCE_REST {
            let q = [("symbol", binance_symbol(sec)), ("limit", limit.to_string())];
            match self.get(&format!("{host}/api/v3/depth"), &q).await.and_then(|j| parse_binance_depth(&j, key)) {
                Ok(mut b) => {
                    b.bids.truncate(depth);
                    b.asks.truncate(depth);
                    return Ok(b);
                }
                Err(e) => last_err = Some(e),
            }
        }
        let product = coinbase_product(sec);
        match self.get(&format!("{COINBASE_REST}/products/{product}/book"), &[("level", "2".into())]).await {
            Ok(j) => parse_coinbase_book(&j, key, depth),
            Err(e) => Err(last_err.unwrap_or(e)),
        }
    }

    pub async fn history(&self, sec: &Security, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let symbol = binance_symbol(sec);
        let mut last_err = None;
        for host in BINANCE_REST {
            match self.binance_history(host, &symbol, tf, end, limit).await {
                Ok(b) if !b.is_empty() => return Ok(b),
                Ok(_) => last_err = Some(anyhow!("{host}: no klines for {symbol}")),
                Err(e) => last_err = Some(e),
            }
        }
        match self.coinbase_history(&coinbase_product(sec), tf, end, limit).await {
            Ok(b) if !b.is_empty() => Ok(b),
            Ok(_) => Err(last_err.unwrap_or_else(|| anyhow!("no crypto data for {}", sec.ticker))),
            Err(e) => Err(e.context(format!("binance: {}", last_err.map(|e| e.to_string()).unwrap_or_default()))),
        }
    }

    /// Quotes keyed by the caller's market keys; change is versus 24 hours ago.
    pub async fn quotes(&self, secs: &[(String, Security)]) -> Result<Vec<Quote>> {
        let symbols: Vec<String> = secs.iter().map(|(_, s)| binance_symbol(s)).collect();
        let list = serde_json::to_string(&symbols)?;
        let mut rows = None;
        for host in BINANCE_REST {
            if let Ok(t) = self.get(&format!("{host}/api/v3/ticker/24hr"), &[("symbols", list.clone())]).await
                && let Ok(r) = parse_binance_24hr(&t)
            {
                rows = Some(r);
                break;
            }
        }
        let now = chrono::Utc::now().timestamp();
        let mut out = Vec::new();
        for (key, sec) in secs {
            let sym = binance_symbol(sec);
            let hit = rows.as_ref().and_then(|r| r.iter().find(|(s, _, _)| *s == sym).map(|(_, l, o)| (*l, *o)));
            let (last, prev) = match hit {
                Some(x) => x,
                None => match self.get(&format!("{COINBASE_REST}/products/{}/stats", coinbase_product(sec)), &[]).await
                {
                    Ok(t) => parse_coinbase_stats(&t)?,
                    Err(_) => continue,
                },
            };
            // Daily closes for the sparkline (best effort).
            let spark = self
                .history(sec, Timeframe::D1, None, 8)
                .await
                .map(|b| b.iter().map(|x| x.close).collect())
                .unwrap_or_default();
            out.push(Quote { symbol: key.clone(), last, prev_close: prev, ts: now, spark });
        }
        if out.is_empty() && !secs.is_empty() {
            bail!("no crypto quotes available");
        }
        Ok(out)
    }

    /// Live bars over websocket, reconnecting with backoff and rotating endpoints.
    pub fn live(sec: &Security, tf: Timeframe) -> LiveFeed {
        let (tx, rx) = mpsc::channel(256);
        let stream = format!("{}@kline_{}", binance_symbol(sec).to_ascii_lowercase(), binance_interval(tf));
        let product = coinbase_product(sec);
        rt::spawn(async move {
            let mut backoff = 1;
            let mut attempt = 0usize;
            while !tx.is_closed() {
                let res = match attempt % 3 {
                    i @ (0 | 1) => binance_ws(&format!("{}/{stream}", BINANCE_WS[i]), &tx).await,
                    _ => coinbase_ws(&product, tf, &tx).await,
                };
                if tx.is_closed() {
                    break;
                }
                if let Err(e) = res {
                    tracing::debug!("live feed {stream}: {e:#}");
                }
                attempt += 1;
                rt::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        });
        rx
    }
}

async fn binance_ws(url: &str, tx: &mpsc::Sender<Bar>) -> Result<()> {
    let mut ws = Socket::connect(url).await.context("binance websocket")?;
    while let Some(text) = ws.next().await {
        if let Some(bar) = parse_binance_ws(&text?)
            && tx.send(bar).await.is_err()
        {
            return Ok(());
        }
    }
    bail!("binance websocket closed")
}

async fn coinbase_ws(product: &str, tf: Timeframe, tx: &mpsc::Sender<Bar>) -> Result<()> {
    let mut ws = Socket::connect(COINBASE_WS).await.context("coinbase websocket")?;
    let sub = serde_json::json!({"type": "subscribe", "product_ids": [product], "channels": ["ticker"]});
    ws.send(sub.to_string()).await?;
    // Aggregate ticks into the current bar; the chart merges partial bars into history.
    let mut bar: Option<Bar> = None;
    while let Some(text) = ws.next().await {
        let Some((ts, px, size)) = parse_coinbase_ws(&text?) else { continue };
        let bucket = tf.bucket_start(ts);
        let b = match bar.as_mut() {
            Some(b) if b.ts == bucket => {
                b.high = b.high.max(px);
                b.low = b.low.min(px);
                b.close = px;
                b.volume += size;
                *b
            }
            _ => *bar.insert(Bar::new(bucket, px, px, px, px, size)),
        };
        if tx.send(b).await.is_err() {
            return Ok(());
        }
    }
    bail!("coinbase websocket closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_book_fixtures() {
        let b = parse_binance_depth(include_str!("../tests/fixtures/binance_depth.json"), "BTCUSD Curncy").unwrap();
        assert_eq!((b.bids.len(), b.asks.len()), (5, 5));
        assert!(b.bids.windows(2).all(|w| w[0].0 > w[1].0));
        assert!(b.asks.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(b.spread().unwrap() > 0.0 && !b.simulated);
        let c = parse_coinbase_book(include_str!("../tests/fixtures/coinbase_book.json"), "BTCUSD Curncy", 3).unwrap();
        assert_eq!((c.bids.len(), c.asks.len()), (3, 3));
        assert!(c.bids[0].0 < c.asks[0].0);
        assert_eq!(c.source, "Coinbase");
    }
    use tr_core::AssetClass;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fix(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}/{name}")).unwrap()
    }

    #[test]
    fn symbols() {
        let btc = Security::new("BTCUSD", AssetClass::Crypto);
        assert_eq!(binance_symbol(&btc), "BTCUSDT");
        assert_eq!(coinbase_product(&btc), "BTC-USD");
        let pepe = Security::new("PEPEUSD", AssetClass::Crypto);
        assert_eq!(binance_symbol(&pepe), "PEPEUSDT");
        assert_eq!(coinbase_product(&pepe), "PEPE-USD");
        assert_eq!(binance_interval(Timeframe::MN1), "1M");
    }

    #[test]
    fn binance_fixtures() {
        let bars = parse_binance_klines(&fix("binance_klines.json")).unwrap();
        assert_eq!(bars.len(), 5);
        assert!(bars.windows(2).all(|w| w[1].ts - w[0].ts == 3600));
        assert!(bars.iter().all(|b| b.low <= b.close && b.high >= b.close && b.close > 1000.0));
        let t = parse_binance_24hr(&fix("binance_24hr.json")).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].0, "BTCUSDT");
        assert!(t[0].1 > 0.0 && t[0].2 > 0.0);
        let k = parse_binance_ws(&fix("binance_ws_kline.json")).unwrap();
        assert_eq!(k.ts, 1_791_778_980);
        assert_eq!(k.close, 83592.01);
        assert!(parse_binance_ws("{\"result\":null}").is_none());
    }

    #[test]
    fn coinbase_fixtures() {
        let bars = parse_coinbase_candles(&fix("coinbase_candles.json")).unwrap();
        assert_eq!(bars.len(), 5);
        assert!(bars.windows(2).all(|w| w[0].ts < w[1].ts));
        assert!(bars.iter().all(|b| b.low <= b.open && b.high >= b.open));
        let (last, open) = parse_coinbase_stats(&fix("coinbase_stats.json")).unwrap();
        assert_eq!((last, open), (2543.56, 2504.69));
        let (ts, px, size) = parse_coinbase_ws(&fix("coinbase_ws_ticker.json")).unwrap();
        assert_eq!(px, 83601.17);
        assert_eq!(size, 0.0012);
        assert_eq!(ts, 1_791_777_838);
        assert!(parse_coinbase_ws("{\"type\":\"subscriptions\"}").is_none());
    }
}
