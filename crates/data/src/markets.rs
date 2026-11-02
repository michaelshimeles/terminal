//! Routes market keys to the free source for each asset class.
//!
//! | class | history | quotes | live |
//! |---|---|---|---|
//! | Crypto | Binance klines (Coinbase fallback) | Binance 24h ticker | Binance kline websocket (Coinbase ticker fallback) |
//! | Govt | Treasury.gov daily curve (FRED fallback); Yahoo `^TNX` etc. intraday | Treasury.gov | — |
//! | Equity/ETF | Alpaca if keyed, else Yahoo | Yahoo spark | — |
//! | FX/Index/Comdty | Yahoo (`EURUSD=X`, `^GSPC`, `CL=F`) | Yahoo spark | — |
//! | HIP-3 perps (`xyz:AAPL`) | — | — | — |
//!
//! HIP-3 perps come only from the Terminal server ([`crate::remote::RemoteMarkets`]).

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use tr_core::{AssetClass, Bar, Security, Timeframe, catalog};

use crate::alpaca::AlpacaProvider;
use crate::crypto::CryptoProvider;
use crate::treasury::TreasuryProvider;
use crate::yahoo::{YahooProvider, yahoo_symbol};
use crate::{
    LiveFeed, MarketData, NewsItem, OptionChain, OrderBook, Quote, SearchHit, YieldCurves, perp_needs_server, security,
};

pub struct Markets {
    yahoo: YahooProvider,
    crypto: CryptoProvider,
    treasury: TreasuryProvider,
    alpaca: Option<AlpacaProvider>,
    name: String,
}

impl Markets {
    pub fn new(alpaca: Option<AlpacaProvider>) -> Result<Self> {
        let name = if alpaca.is_some() { "alpaca+yahoo+binance+ust" } else { "yahoo+binance+ust" }.to_string();
        Ok(Self {
            yahoo: YahooProvider::new()?,
            crypto: CryptoProvider::new()?,
            treasury: TreasuryProvider::new()?,
            alpaca,
            name,
        })
    }
}

#[async_trait]
impl MarketData for Markets {
    fn name(&self) -> &str {
        &self.name
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let sec = security(symbol);
        match sec.class {
            AssetClass::Perp => Err(perp_needs_server(symbol)),
            AssetClass::Crypto => self.crypto.history(&sec, tf, end, limit).await,
            AssetClass::Govt if tf.is_intraday() => match catalog::by_security(&sec).and_then(|l| l.yahoo) {
                Some(y) => self.yahoo.history(y, tf, end, limit).await,
                None => bail!("intraday yields are not available for {}; use D, W or M", sec.ticker),
            },
            AssetClass::Govt => self.treasury.history(&sec, tf, end, limit).await,
            AssetClass::Equity | AssetClass::Etf if self.alpaca.is_some() => {
                let a = self.alpaca.as_ref().expect("checked");
                match a.history(&sec.ticker, tf, end, limit).await {
                    Ok(b) if !b.is_empty() => Ok(b),
                    _ => self.yahoo.history(&yahoo_symbol(&sec), tf, end, limit).await,
                }
            }
            _ => self.yahoo.history(&yahoo_symbol(&sec), tf, end, limit).await,
        }
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let (mut crypto, mut govt, mut other) = (Vec::new(), Vec::new(), Vec::new());
        for key in symbols {
            let sec = security(key);
            match sec.class {
                AssetClass::Perp => {}
                AssetClass::Crypto => crypto.push((key.clone(), sec)),
                AssetClass::Govt => govt.push((key.clone(), sec)),
                _ => other.push((key.clone(), sec)),
            }
        }
        let (c, g, y) = tokio::join!(
            async { if crypto.is_empty() { Ok(vec![]) } else { self.crypto.quotes(&crypto).await } },
            async { if govt.is_empty() { Ok(vec![]) } else { self.treasury.quotes(&govt).await } },
            async {
                if other.is_empty() {
                    return Ok(vec![]);
                }
                let ys: Vec<String> = other.iter().map(|(_, s)| yahoo_symbol(s)).collect();
                let quotes = self.yahoo.quotes(&ys).await?;
                // Map Yahoo symbols back to the caller's market keys.
                Ok::<_, anyhow::Error>(
                    other
                        .iter()
                        .zip(&ys)
                        .filter_map(|((key, _), y)| {
                            quotes.iter().find(|q| &q.symbol == y).map(|q| Quote { symbol: key.clone(), ..q.clone() })
                        })
                        .collect(),
                )
            }
        );
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for r in [c, g, y] {
            match r {
                Ok(q) => out.extend(q),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if out.is_empty() && !errors.is_empty() {
            return Err(anyhow!(errors.join("; ")));
        }
        Ok(out)
    }

    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        let sec = security(symbol);
        (sec.class == AssetClass::Crypto).then(|| CryptoProvider::live(&sec, tf))
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.yahoo.search(query).await
    }

    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        let q = match symbol.map(security) {
            Some(sec) => match sec.class {
                // Yahoo has no crypto headlines; use CoinDesk's feed.
                AssetClass::Crypto => return self.crypto.news(&sec).await,
                // A linked perp shares its underlying's headlines.
                AssetClass::Perp => match tr_core::perps::link_for(&sec.ticker) {
                    Some(l) => yahoo_symbol(&security(l.underlying)),
                    None => return Ok(Vec::new()),
                },
                AssetClass::Govt => catalog::by_security(&sec).and_then(|l| l.yahoo).unwrap_or("^TNX").to_string(),
                _ => yahoo_symbol(&sec),
            },
            None => "stock market".into(),
        };
        self.yahoo.news(&q).await
    }

    /// Listed options on US equities, ETFs and indices (Yahoo).
    async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<OptionChain> {
        let sec = security(symbol);
        match sec.class {
            AssetClass::Equity | AssetClass::Etf | AssetClass::Index => {
                let y = yahoo_symbol(&sec);
                // Yahoo lists S&P 500 index options under ^SPX, not the ^GSPC quote symbol.
                let y = if y == "^GSPC" { "^SPX".to_string() } else { y };
                self.yahoo.options(&y, expiration).await
            }
            _ => bail!("listed options are available for US equities, ETFs and indices, not {}", sec.key()),
        }
    }

    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        let sec = security(symbol);
        match sec.class {
            AssetClass::Crypto => self.crypto.order_book(symbol, &sec, depth).await,
            _ => bail!("no free public order book for {}", sec.key()),
        }
    }

    async fn yield_curves(&self) -> Result<YieldCurves> {
        self.treasury.curves().await
    }
}

/// Market key for a security (convenience for callers holding a [`Security`]).
pub fn key(sec: &Security) -> String {
    sec.key()
}
