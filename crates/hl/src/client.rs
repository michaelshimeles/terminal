//! `/info` and `/exchange` on testnet, over reqwest (native, and `fetch` in the browser).
//!
//! The client has no base-URL setting: it always talks to [`crate::network::API_URL`]. (Unit
//! tests point it at a local mock server through a `cfg(test)` constructor that the app
//! can't call.) Logs carry the action type, nonce, cloid, oid and status, never a request
//! body or a signature.

use alloy_primitives::Address;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::assets::{AssetRegistry, SUPPORTED_DEXES};
use crate::key::address_hex;
use crate::model::*;
use crate::network::Network;
use crate::trader::Signed;

/// One order's or cancel's result.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Resting {
        oid: u64,
    },
    Filled {
        oid: u64,
        total_sz: f64,
        avg_px: f64,
    },
    /// A cancel, modify or other action that went through.
    Success,
    Error(String),
    Other(String),
}

/// What `/exchange` said.
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeResponse {
    /// `status: "ok"`. Individual orders can still have failed: see `statuses`.
    pub ok: bool,
    /// `order`, `cancel`, `default`, …
    pub kind: Option<String>,
    pub statuses: Vec<Status>,
    /// The whole request was refused (`status: "err"`).
    pub error: Option<String>,
}

impl ExchangeResponse {
    pub fn parse(v: &Value) -> Self {
        let ok = v["status"].as_str() == Some("ok");
        let error = (!ok).then(|| match &v["response"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let statuses = v["response"]["data"]["statuses"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                if s.as_str() == Some("success") {
                    Status::Success
                } else if let Some(e) = s["error"].as_str() {
                    Status::Error(e.to_string())
                } else if let Some(oid) = s["resting"]["oid"].as_u64() {
                    Status::Resting { oid }
                } else if let Some(oid) = s["filled"]["oid"].as_u64() {
                    Status::Filled {
                        oid,
                        total_sz: num(&s["filled"]["totalSz"]).unwrap_or(0.0),
                        avg_px: num(&s["filled"]["avgPx"]).unwrap_or(0.0),
                    }
                } else {
                    Status::Other(s.to_string())
                }
            })
            .collect();
        Self { ok, kind: v["response"]["type"].as_str().map(str::to_string), statuses, error }
    }

    /// The first error, whether for the request or for an order in it.
    pub fn first_error(&self) -> Option<&str> {
        self.error.as_deref().or_else(|| {
            self.statuses.iter().find_map(|s| match s {
                Status::Error(e) => Some(e.as_str()),
                _ => None,
            })
        })
    }

    /// One line for a toast or a log, addresses masked: `ok (order): resting oid 123`, `err: …`.
    pub fn summary(&self) -> String {
        crate::key::mask_addresses(&self.summary_raw())
    }

    fn summary_raw(&self) -> String {
        if let Some(e) = &self.error {
            return format!("err: {e}");
        }
        let parts: Vec<String> = self
            .statuses
            .iter()
            .map(|s| match s {
                Status::Resting { oid } => format!("resting oid {oid}"),
                Status::Filled { oid, total_sz, avg_px } => format!("filled {total_sz} @ {avg_px} (oid {oid})"),
                Status::Success => "success".into(),
                Status::Error(e) => format!("error: {e}"),
                Status::Other(o) => o.clone(),
            })
            .collect();
        format!(
            "ok ({}){}",
            self.kind.as_deref().unwrap_or("?"),
            if parts.is_empty() { String::new() } else { format!(": {}", parts.join("; ")) }
        )
    }
}

/// The address Hyperliquid recovered from a signature it doesn't recognise: "User or API
/// Wallet 0x… does not exist." A match with the signer means the signature is right and the
/// account is just unknown (never funded); a mismatch means the signing is wrong.
pub fn recovered_address(error: &str) -> Option<Address> {
    let i = error.find("0x")?;
    crate::key::parse_address(error.get(i..i + 42)?).ok()
}

/// Why a send did not clearly succeed.
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    /// Nothing reached Hyperliquid (couldn't connect).
    NotSent(String),
    /// Hyperliquid answered with an HTTP error: nothing was done.
    Rejected { status: u16, body: String },
    /// It may have arrived: check open orders (by cloid) before sending again.
    Uncertain(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NotSent(m) => write!(f, "not sent: {m}"),
            SendError::Rejected { status, body } => write!(f, "rejected (HTTP {status}): {body}"),
            SendError::Uncertain(m) => write!(f, "status unknown: {m}"),
        }
    }
}

impl std::error::Error for SendError {}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    info_url: String,
    exchange_url: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_urls(Network::Testnet.info_url(), Network::Testnet.exchange_url())
    }

    /// Unit tests only: a local mock server.
    #[cfg(test)]
    pub(crate) fn mock(base: &str) -> Result<Self> {
        Self::with_urls(format!("{base}/info"), format!("{base}/exchange"))
    }

    fn with_urls(info_url: String, exchange_url: String) -> Result<Self> {
        let b = reqwest::Client::builder();
        #[cfg(not(target_arch = "wasm32"))]
        let b =
            b.timeout(std::time::Duration::from_secs(15)).user_agent(concat!("terminal/", env!("CARGO_PKG_VERSION")));
        Ok(Self { http: b.build()?, info_url, exchange_url })
    }

    /// Any `/info` query.
    pub async fn info(&self, body: Value) -> Result<Value> {
        let kind = body["type"].as_str().unwrap_or("?").to_string();
        let r = self.http.post(&self.info_url).json(&body).send().await.with_context(|| format!("info {kind}"))?;
        let status = r.status();
        let text = r.text().await.with_context(|| format!("info {kind}"))?;
        if !status.is_success() {
            return Err(anyhow!("info {kind}: HTTP {status}: {}", text.chars().take(200).collect::<String>()));
        }
        serde_json::from_str(&text).with_context(|| format!("info {kind}: not JSON"))
    }

    /// The asset ids and rules for the core and `xyz` dexes, read fresh.
    pub async fn registry(&self) -> Result<AssetRegistry> {
        let (dexes, core) = futures::join!(self.info(json!({"type": "perpDexs"})), self.info(json!({"type": "meta"})));
        let (dexes, core) = (dexes?, core?);
        let mut metas = Vec::new();
        for dex in SUPPORTED_DEXES {
            metas.push((dex, self.info(json!({"type": "meta", "dex": dex})).await?));
        }
        let refs: Vec<(&str, &Value)> = metas.iter().map(|(d, m)| (*d, m)).collect();
        AssetRegistry::from_info(&dexes, &core, &refs)
    }

    pub async fn clearinghouse(&self, user: &Address, dex: &str) -> Result<Clearinghouse> {
        let v = self.info(json!({"type": "clearinghouseState", "user": address_hex(user), "dex": dex})).await?;
        Ok(parse_clearinghouse(dex, &v))
    }

    pub async fn open_orders(&self, user: &Address, dex: &str) -> Result<Vec<OpenOrder>> {
        let v = self.info(json!({"type": "frontendOpenOrders", "user": address_hex(user), "dex": dex})).await?;
        Ok(parse_open_orders(&v))
    }

    pub async fn asset_data(&self, user: &Address, coin: &str) -> Result<Option<AssetData>> {
        let v = self.info(json!({"type": "activeAssetData", "user": address_hex(user), "coin": coin})).await?;
        Ok(parse_asset_data(&v))
    }

    pub async fn mids(&self, dex: &str) -> Result<std::collections::HashMap<String, f64>> {
        let v = self.info(json!({"type": "allMids", "dex": dex})).await?;
        Ok(v.as_object().into_iter().flatten().filter_map(|(k, p)| Some((k.clone(), num(p)?))).collect())
    }

    /// `missing`, `user`, `agent`, `vault` or `subAccount`.
    pub async fn user_role(&self, user: &Address) -> Result<String> {
        let v = self.info(json!({"type": "userRole", "user": address_hex(user)})).await?;
        Ok(v["role"].as_str().unwrap_or("unknown").to_string())
    }

    pub async fn extra_agents(&self, user: &Address) -> Result<Vec<ExtraAgent>> {
        Ok(parse_extra_agents(&self.info(json!({"type": "extraAgents", "user": address_hex(user)})).await?))
    }

    /// Post a signed L1 action.
    pub async fn send(&self, signed: &Signed) -> Result<ExchangeResponse, SendError> {
        tracing::info!(kind = signed.kind, nonce = signed.nonce, "hyperliquid testnet: sending");
        let r = self.post_exchange(&signed.body()).await;
        match &r {
            Ok(resp) => {
                tracing::info!(kind = signed.kind, nonce = signed.nonce, "hyperliquid testnet: {}", resp.summary())
            }
            Err(e) => tracing::warn!(
                kind = signed.kind,
                nonce = signed.nonce,
                "hyperliquid testnet: {}",
                crate::key::mask_addresses(&e.to_string())
            ),
        }
        r
    }

    /// Post a user-signed action body (`approveAgent`).
    pub async fn send_user_signed(&self, body: &Value) -> Result<ExchangeResponse, SendError> {
        let kind = body["action"]["type"].as_str().unwrap_or("?").to_string();
        tracing::info!(kind, "hyperliquid testnet: sending");
        let r = self.post_exchange(body).await;
        if let Ok(resp) = &r {
            tracing::info!(kind, "hyperliquid testnet: {}", resp.summary());
        }
        r
    }

    async fn post_exchange(&self, body: &Value) -> Result<ExchangeResponse, SendError> {
        let r = self.http.post(&self.exchange_url).json(body).send().await.map_err(classify)?;
        let status = r.status().as_u16();
        let text = r.text().await.map_err(|e| SendError::Uncertain(e.to_string()))?;
        if (400..500).contains(&status) {
            return Err(SendError::Rejected { status, body: text.chars().take(300).collect() });
        }
        if status >= 500 {
            return Err(SendError::Uncertain(format!("HTTP {status}")));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|_| SendError::Uncertain(format!("unreadable answer: {text}")))?;
        Ok(ExchangeResponse::parse(&v))
    }
}

/// A failure before any answer: certainly not sent only if the connection never opened.
/// The browser's `fetch` doesn't say, so there every such failure is uncertain.
fn classify(e: reqwest::Error) -> SendError {
    #[cfg(not(target_arch = "wasm32"))]
    if e.is_connect() || e.is_builder() {
        return SendError::NotSent(e.to_string());
    }
    SendError::Uncertain(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_parse() {
        let resting = ExchangeResponse::parse(
            &json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [{"resting": {"oid": 77738308}}]}}}),
        );
        assert_eq!(resting.statuses, [Status::Resting { oid: 77738308 }]);
        assert_eq!(resting.summary(), "ok (order): resting oid 77738308");
        let min = ExchangeResponse::parse(
            &json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [{"error": "Order must have minimum value of $10."}]}}}),
        );
        assert!(min.ok && min.first_error() == Some("Order must have minimum value of $10."));
        let filled = ExchangeResponse::parse(
            &json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [{"filled": {"totalSz": "0.02", "avgPx": "1891.4", "oid": 77747314}}]}}}),
        );
        assert_eq!(filled.statuses, [Status::Filled { oid: 77747314, total_sz: 0.02, avg_px: 1891.4 }]);
        let cancel = ExchangeResponse::parse(
            &json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": ["success"]}}}),
        );
        assert_eq!(cancel.statuses, [Status::Success]);
        let err = ExchangeResponse::parse(
            &json!({"status": "err", "response": "User or API Wallet 0x5e9ee1089755c3435139848e47e6635505d5a13a does not exist."}),
        );
        assert!(!err.ok);
        assert_eq!(
            recovered_address(err.first_error().unwrap()).map(|a| address_hex(&a)).as_deref(),
            Some("0x5e9ee1089755c3435139848e47e6635505d5a13a")
        );
        assert_eq!(
            ExchangeResponse::parse(&json!({"status": "ok", "response": {"type": "default"}})).summary(),
            "ok (default)"
        );
        assert_eq!(recovered_address("Insufficient margin"), None);
    }
}
