//! Market data for the browser build.
//!
//! A page can only read sources that allow cross-origin requests. Binance and Coinbase do,
//! so crypto is live: REST history, quotes and order books, and websocket bars. Yahoo,
//! Treasury.gov, Alpaca and CoinDesk do not, so every other asset class comes from the
//! sample generator and there are no headlines or network search.
//!
//! When the page has a Terminal server ([`server_url`]), [`crate::for_browser`] puts
//! [`crate::remote::RemoteMarkets`] in front of these, so crypto comes from the server and
//! from Binance and Coinbase only while the server can't serve it.
//!
//! TODO(web proxy): route equities, FX, indices, futures, Treasuries and news through a
//! same-origin proxy to those sources, then use [`crate::markets::Markets`] here instead.

use anyhow::Result;
use async_trait::async_trait;
use tr_core::{AssetClass, Bar, Timeframe};

use crate::crypto::CryptoProvider;
use crate::sample::SampleProvider;
use crate::{LiveFeed, MarketData, OrderBook, Quote, YieldCurves, security};

pub struct WebMarkets {
    crypto: CryptoProvider,
    sample: SampleProvider,
}

impl WebMarkets {
    pub fn new() -> Result<Self> {
        Ok(Self { crypto: CryptoProvider::new()?, sample: SampleProvider::default() })
    }
}

/// The Terminal server a page uses for crypto markets: `built`, the URL set when the page was
/// built, unless the page's `?server=` parameter (`requested`) turns it off (`off`) or names a
/// server on the same machine, such as `ws://localhost:8787/ws`. Other servers are refused, so a
/// link can't point someone's page at a server that isn't ours.
pub fn server_url(built: Option<&str>, requested: Option<&str>) -> Option<String> {
    let built = built.map(str::trim).filter(|u| !u.is_empty()).map(str::to_string);
    let Some(requested) = requested.map(str::trim) else { return built };
    if requested.eq_ignore_ascii_case("off") {
        return None;
    }
    if is_local(requested) {
        return Some(requested.to_string());
    }
    tracing::warn!("?server= takes a server on this machine (ws://localhost:8787/ws) or off; ignoring it");
    built
}

/// A `ws://` or `wss://` URL whose host is this machine. Anything unusual (user info, other
/// spellings of the address) counts as not local.
fn is_local(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://").or_else(|| url.strip_prefix("wss://")) else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn is_crypto(symbol: &str) -> bool {
    security(symbol).class == AssetClass::Crypto
}

#[async_trait]
impl MarketData for WebMarkets {
    fn name(&self) -> &str {
        "binance+sample"
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        if is_crypto(symbol) {
            self.crypto.history(&security(symbol), tf, end, limit).await
        } else {
            self.sample.history(symbol, tf, end, limit).await
        }
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let (crypto, other): (Vec<String>, Vec<String>) = symbols.iter().cloned().partition(|s| is_crypto(s));
        let mut out = self.sample.quotes(&other).await?;
        if !crypto.is_empty() {
            let secs: Vec<_> = crypto.iter().map(|k| (k.clone(), security(k))).collect();
            match self.crypto.quotes(&secs).await {
                Ok(q) => out.extend(q),
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => tracing::warn!("crypto quotes: {e:#}"),
            }
        }
        Ok(out)
    }

    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        is_crypto(symbol).then(|| CryptoProvider::live(&security(symbol), tf))
    }

    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        let sec = security(symbol);
        if sec.class == AssetClass::Crypto {
            self.crypto.order_book(symbol, &sec, depth).await
        } else {
            anyhow::bail!("no public order book for {} in the browser", sec.key())
        }
    }

    async fn yield_curves(&self) -> Result<YieldCurves> {
        self.sample.yield_curves().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_crypto_comes_from_sample_data() {
        let m = WebMarkets::new().unwrap();
        let bars = m.history("AAPL US Equity", Timeframe::D1, None, 50).await.unwrap();
        assert_eq!(bars.len(), 50);
        let q = m.quotes(&["AAPL US Equity".into(), "EURUSD Curncy".into()]).await.unwrap();
        assert_eq!(q.len(), 2);
        assert!(m.live("AAPL US Equity", Timeframe::M1).is_none());
        assert!(m.order_book("AAPL US Equity", 10).await.is_err());
        assert_eq!(m.yield_curves().await.unwrap().current.points.len(), 11);
        assert!(is_crypto("BTCUSD Curncy") && !is_crypto("US10Y Govt"));
    }

    #[test]
    fn server_url_from_the_build_or_a_local_override() {
        let prod = Some("wss://terminal-gateway.style.dev/ws");
        assert_eq!(server_url(prod, None).as_deref(), prod);
        assert_eq!(server_url(Some(" "), None), None, "an empty build setting means no server");
        assert_eq!(server_url(None, None), None);
        assert_eq!(server_url(prod, Some("off")), None);
        for local in ["ws://localhost:8787/ws", "ws://127.0.0.1:9", "wss://[::1]:8787/ws", "ws://LOCALHOST/ws"] {
            assert_eq!(server_url(prod, Some(local)).as_deref(), Some(local));
            assert_eq!(server_url(None, Some(local)).as_deref(), Some(local));
        }
        for refused in [
            "wss://evil.example/ws",
            "ws://localhost.evil.example/ws",
            "ws://localhost@evil.example/ws",
            "ws://evil.example:80@localhost/ws",
            "http://localhost:8787/ws",
            "",
        ] {
            assert_eq!(server_url(prod, Some(refused)).as_deref(), prod, "{refused}");
            assert_eq!(server_url(None, Some(refused)), None, "{refused}");
        }
    }

    #[tokio::test]
    async fn the_server_goes_in_front_of_the_browser_sources() {
        assert_eq!(crate::for_browser(false, None).name(), "binance+sample");
        assert_eq!(crate::for_browser(false, Some("ws://127.0.0.1:9/ws")).name(), "server+binance+sample");
        assert_eq!(crate::for_browser(true, Some("ws://127.0.0.1:9/ws")).name(), "sample");
    }
}
