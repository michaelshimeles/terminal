//! The Turnkey calls Terminal makes, stamped with the device's session key: who am I, which
//! Ethereum account the wallet has, and signing one 32-byte digest.
//!
//! Requests follow Turnkey's public API (`POST /public/v1/query/*` and
//! `/public/v1/submit/*`, a JSON body, and an `X-Stamp` header signed over that exact body),
//! as `turnkey_client` 0.17 builds them. That client isn't used directly: it waits with
//! tokio timers, which don't run in the browser, and pulls a second `reqwest`.
//!
//! [`Client::sign_digest`] sends the digest as `alloy-signer-turnkey` does:
//! `ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2` with `PAYLOAD_ENCODING_HEXADECIMAL` and
//! `HASH_FUNCTION_NO_OP`, so Turnkey signs those 32 bytes as they are.

use std::time::Duration;

use alloy_primitives::{Address, B256, Signature, U256};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::Session;

/// Turnkey's API.
pub const API_URL: &str = "https://api.turnkey.com";
/// The longest any one request may take, on the desktop and in the browser.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The wallet account Terminal uses: the first Ethereum account, as the sign-up creates it.
pub const ETHEREUM_PATH: &str = "m/44'/60'/0'/0/0";

/// `url` if it is a plain-HTTP loopback address (`http://127.0.0.1:<port>` or
/// `http://localhost:<port>`): the only kind of override Terminal accepts, for a local mock.
pub fn loopback_only(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("http://")?;
    let host_port = rest.split('/').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    let port_ok = port.parse::<u16>().is_ok_and(|p| p != 0);
    ((host == "127.0.0.1" || host == "localhost") && port_ok).then(|| url.trim().trim_end_matches('/').to_string())
}

/// Turnkey's API, unless `TR_TURNKEY_API_URL` names a local mock (desktop only).
pub fn api_url() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(u) = std::env::var("TR_TURNKEY_API_URL").ok().and_then(|u| loopback_only(&u)) {
        tracing::warn!("turnkey: using a local mock API at {u}");
        return u;
    }
    API_URL.to_string()
}

/// `whoami`: the user and organization a session belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub organization_id: String,
    #[serde(default)]
    pub organization_name: String,
    pub user_id: String,
    #[serde(default)]
    pub username: String,
}

/// A wallet account (an address Turnkey holds the key for).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletAccount {
    pub wallet_id: String,
    pub address: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub address_format: String,
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    /// Turnkey's API (or a local mock, see [`api_url`]).
    pub fn new() -> Result<Self> {
        Self::with_base(&api_url())
    }

    /// Any base URL (tests point this at the mock).
    pub fn with_base(base: &str) -> Result<Self> {
        let b = reqwest::Client::builder();
        #[cfg(not(target_arch = "wasm32"))]
        let b =
            b.user_agent(concat!("terminal/", env!("CARGO_PKG_VERSION"))).redirect(reqwest::redirect::Policy::none());
        Ok(Self { http: b.build()?, base: base.trim_end_matches('/').to_string() })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// POST `body` to `path`, stamped with the session's key. The stamp signs these exact
    /// bytes, so the body is serialized once and sent as is.
    async fn post(&self, s: &Session, path: &str, body: &Value) -> Result<Value> {
        let text = serde_json::to_string(body)?;
        let (name, value) = s.key.stamp(text.as_bytes())?;
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(REQUEST_TIMEOUT)
            .header("content-type", "application/json")
            .header(name, value)
            .body(text)
            .send()
            .await
            .with_context(|| format!("turnkey {path}"))?;
        let status = r.status();
        let text = r.text().await.with_context(|| format!("turnkey {path}"))?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            bail!("turnkey {path}: HTTP {}: {msg}", status.as_u16());
        }
        serde_json::from_str(&text).with_context(|| format!("turnkey {path}: not JSON"))
    }

    pub async fn whoami(&self, s: &Session) -> Result<WhoAmI> {
        let v = self.post(s, "/public/v1/query/whoami", &json!({"organizationId": s.organization_id()})).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// The wallet's Ethereum account at [`ETHEREUM_PATH`] (or, failing that, its first
    /// Ethereum account). Its address is the user's Hyperliquid account.
    pub async fn ethereum_account(&self, s: &Session) -> Result<WalletAccount> {
        let org = s.organization_id();
        let wallets = self.post(s, "/public/v1/query/list_wallets", &json!({"organizationId": org})).await?;
        let mut accounts = Vec::new();
        for w in wallets["wallets"].as_array().into_iter().flatten() {
            let Some(id) = w["walletId"].as_str() else { continue };
            let v = self
                .post(s, "/public/v1/query/list_wallet_accounts", &json!({"organizationId": org, "walletId": id}))
                .await?;
            let list: Vec<WalletAccount> = serde_json::from_value(v["accounts"].clone()).unwrap_or_default();
            accounts.extend(list);
        }
        pick_ethereum(accounts).ok_or_else(|| anyhow!("the Turnkey wallet has no Ethereum account"))
    }

    /// Have the wallet at `sign_with` sign `digest` as it is (no hashing, no prefix).
    pub async fn sign_digest(&self, s: &Session, sign_with: &Address, digest: &B256) -> Result<Signature> {
        let body = sign_raw_payload_body(s.organization_id(), sign_with, digest, now_ms());
        let activity = self.submit(s, "/public/v1/submit/sign_raw_payload", &body).await?;
        let result = &activity["result"]["signRawPayloadResult"];
        parse_signature(result)
    }

    /// Submit an activity and return it once Turnkey has completed it.
    async fn submit(&self, s: &Session, path: &str, body: &Value) -> Result<Value> {
        let mut v = self.post(s, path, body).await?;
        // A single-user organization completes at once; ask again a few times if not.
        for _ in 0..3 {
            match v["activity"]["status"].as_str() {
                Some("ACTIVITY_STATUS_PENDING") | Some("ACTIVITY_STATUS_CREATED") => {
                    let id = v["activity"]["id"].as_str().unwrap_or_default().to_string();
                    v = self
                        .post(
                            s,
                            "/public/v1/query/get_activity",
                            &json!({"organizationId": s.organization_id(), "activityId": id}),
                        )
                        .await?;
                }
                _ => break,
            }
        }
        let a = &v["activity"];
        match a["status"].as_str() {
            Some("ACTIVITY_STATUS_COMPLETED") => Ok(a.clone()),
            Some("ACTIVITY_STATUS_FAILED") => {
                bail!("Turnkey refused: {}", a["failure"]["message"].as_str().unwrap_or("no reason given"))
            }
            Some("ACTIVITY_STATUS_CONSENSUS_NEEDED") | Some("ACTIVITY_STATUS_AUTHENTICATORS_NEEDED") => {
                bail!("Turnkey needs another approval for this; nothing was signed")
            }
            Some(other) => bail!("Turnkey answered {other}"),
            None => bail!("Turnkey's answer has no activity"),
        }
    }
}

fn now_ms() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The Ethereum account Terminal uses from a wallet's accounts.
pub fn pick_ethereum(accounts: Vec<WalletAccount>) -> Option<WalletAccount> {
    let eth: Vec<WalletAccount> =
        accounts.into_iter().filter(|a| a.address_format == "ADDRESS_FORMAT_ETHEREUM").collect();
    eth.iter().find(|a| a.path == ETHEREUM_PATH).cloned().or_else(|| eth.into_iter().next())
}

/// The `sign_raw_payload` request for a 32-byte digest (what `alloy-signer-turnkey`'s
/// `sign_hash` sends): the payload is the digest in lower-case hex without `0x`, and
/// `signWith` the checksummed address.
pub fn sign_raw_payload_body(org: &str, sign_with: &Address, digest: &B256, timestamp_ms: u64) -> Value {
    json!({
        "type": "ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2",
        "timestampMs": timestamp_ms.to_string(),
        "organizationId": org,
        "parameters": {
            "signWith": sign_with.to_string(),
            "payload": hex::encode(digest),
            "encoding": "PAYLOAD_ENCODING_HEXADECIMAL",
            "hashFunction": "HASH_FUNCTION_NO_OP",
        },
    })
}

/// `{r, s, v}` from `signRawPayloadResult`: 32-byte hex `r` and `s`, and `v` as one hex byte
/// (`00`/`01`, or 27/28).
pub fn parse_signature(result: &Value) -> Result<Signature> {
    let field = |k: &str| -> Result<Vec<u8>> {
        let s = result[k].as_str().ok_or_else(|| anyhow!("the signature has no {k}"))?;
        hex::decode(s.trim_start_matches("0x")).map_err(|_| anyhow!("the signature's {k} is not hex"))
    };
    let (r, s, v) = (field("r")?, field("s")?, field("v")?);
    ensure!(r.len() == 32 && s.len() == 32 && v.len() == 1, "the signature has the wrong length");
    let parity = match v[0] {
        0 | 27 => false,
        1 | 28 => true,
        _ => bail!("the signature's v is not 0, 1, 27 or 28"),
    };
    Ok(Signature::new(U256::from_be_slice(&r), U256::from_be_slice(&s), parity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_overrides_are_accepted() {
        assert_eq!(loopback_only("http://127.0.0.1:8123").as_deref(), Some("http://127.0.0.1:8123"));
        assert_eq!(loopback_only("http://localhost:9/x/").as_deref(), Some("http://localhost:9/x"));
        for bad in [
            "https://api.turnkey.com",
            "http://evil.example:80",
            "http://127.0.0.1.evil.example:80",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://user@127.0.0.1:80",
            "file:///tmp/x",
            "",
        ] {
            assert_eq!(loopback_only(bad), None, "{bad}");
        }
    }

    /// The body `alloy-signer-turnkey` 2.5's `sign_hash` would send for the same digest
    /// (field names from `turnkey_client` 0.17's `SignRawPayloadIntentV2`).
    #[test]
    fn the_sign_request_matches_alloy_signer_turnkey() {
        let addr: Address = "0x14791697260e4c9a71f18484c9f997b308e59325".parse().unwrap();
        let digest = B256::from([0xab; 32]);
        let body = sign_raw_payload_body("org-1", &addr, &digest, 1_700_000_000_000);
        assert_eq!(
            body,
            json!({
                "type": "ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2",
                "timestampMs": "1700000000000",
                "organizationId": "org-1",
                "parameters": {
                    "signWith": "0x14791697260E4c9A71f18484C9f997B308e59325",
                    "payload": "ab".repeat(32),
                    "encoding": "PAYLOAD_ENCODING_HEXADECIMAL",
                    "hashFunction": "HASH_FUNCTION_NO_OP",
                },
            })
        );
    }

    #[test]
    fn signatures_parse_with_either_v() {
        let r = "11".repeat(32);
        let s = "22".repeat(32);
        for (v, odd) in [("00", false), ("01", true), ("1b", false), ("1c", true)] {
            let sig = parse_signature(&json!({"r": r, "s": s, "v": v})).unwrap();
            assert_eq!(sig.v(), odd, "{v}");
            assert_eq!(sig.r(), U256::from_be_slice(&[0x11; 32]));
        }
        assert!(parse_signature(&json!({"r": r, "s": s, "v": "02"})).is_err());
        assert!(parse_signature(&json!({"r": "11", "s": s, "v": "00"})).is_err());
        assert!(parse_signature(&json!({"r": r, "v": "00"})).is_err());
    }

    #[test]
    fn the_ethereum_account_is_the_default_path() {
        let acct = |path: &str, fmt: &str, addr: &str| WalletAccount {
            wallet_id: "w".into(),
            address: addr.into(),
            path: path.into(),
            address_format: fmt.into(),
        };
        let picked = pick_ethereum(vec![
            acct("m/44'/501'/0'/0'", "ADDRESS_FORMAT_SOLANA", "So1"),
            acct("m/44'/60'/0'/0/1", "ADDRESS_FORMAT_ETHEREUM", "0x2"),
            acct(ETHEREUM_PATH, "ADDRESS_FORMAT_ETHEREUM", "0x1"),
        ]);
        assert_eq!(picked.unwrap().address, "0x1");
        assert_eq!(pick_ethereum(vec![acct("m/44'/501'/0'/0'", "ADDRESS_FORMAT_SOLANA", "So1")]), None);
    }
}
