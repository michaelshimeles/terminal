//! Market data for the browser build.
//!
//! A page can only read sources that allow cross-origin requests. Binance and Coinbase do,
//! so crypto is live: REST history, quotes and order books, and websocket bars. Yahoo,
//! Treasury.gov, Alpaca and CoinDesk do not, so every other asset class comes from the
//! sample generator and there are no headlines or network search.
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
}
