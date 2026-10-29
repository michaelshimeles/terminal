//! SnapTrade REST client.
//!
//! Every partner request carries `clientId` and a Unix `timestamp` in the query string and a
//! `Signature` header: base64(HMAC-SHA256(consumerKey, canonical JSON of
//! `{"content": body|null, "path": <URL path>, "query": <raw query string>}`)), with keys
//! sorted at every level and no whitespace. User-scoped calls add `userId` and `userSecret`
//! (omitted for personal API keys). Requests use the canonical root paths on
//! `https://api.snaptrade.com` (the `/api/v1` prefix is deprecated).
//!
//! This module holds the reads, user registration, the read-only Connection Portal link and
//! deleting a connection on explicit request; order entry lives in [`crate::trading`].

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use hmac::{Hmac, Mac};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::model::{self, Portfolio};
use crate::secrets::Credentials;

pub const DEFAULT_BASE: &str = "https://api.snaptrade.com";
/// Customer dashboard where partner keys are created.
pub const DASHBOARD_URL: &str = "https://dashboard.snaptrade.com";
pub const DOCS_URL: &str = "https://docs.snaptrade.com";

/// Days of order history and activity loaded per account.
pub const ORDER_DAYS: u32 = 30;
pub const ACTIVITY_DAYS: i64 = 90;
pub const ACTIVITY_LIMIT: u32 = 100;

/// At most this many SnapTrade requests are in flight at once, across every client in the
/// process. SnapTrade rate-limits per key, and loading many accounts at once (four requests
/// per account) is what trips it.
pub const MAX_CONCURRENT: usize = 4;
/// A throttled read is retried this many times.
pub const MAX_RETRIES: u32 = 3;
/// Longest `Retry-After` honoured inside one call; longer waits go back to the caller.
pub const MAX_INLINE_WAIT: Duration = Duration::from_secs(20);

static IN_FLIGHT: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT));
/// After a throttled answer nothing is sent until this instant, so one `Retry-After` pauses
/// every request rather than each finding out on its own.
static COOLDOWN: Mutex<Option<Instant>> = Mutex::new(None);

pub struct SnapTrade {
    http: reqwest::Client,
    base: Url,
    creds: Credentials,
}

/// Canonical JSON for signing: keys sorted at every level, no whitespace, non-ASCII escaped
/// as `\uXXXX` (matching Python's `json.dumps(..., separators=(",", ":"), sort_keys=True)`).
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_canonical(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        Value::String(s) => write_string(s, out),
        other => out.push_str(&other.to_string()),
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The `Signature` header value for one request.
pub fn signature(consumer_key: &str, path: &str, query: &str, body: Option<&Value>) -> String {
    let content = match body {
        None => Value::Null,
        Some(Value::Object(m)) if m.is_empty() => Value::Null,
        Some(b) => b.clone(),
    };
    let payload = canonical_json(&json!({ "content": content, "path": path, "query": query }));
    let mut mac = Hmac::<Sha256>::new_from_slice(consumer_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// SnapTrade answered with an error status. Displays as the readable message, so callers that
/// only show errors are unaffected; order entry inspects `status` to tell a rejection from an
/// unknown outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Seconds to wait before trying again, from `Retry-After` or the throttle message.
    pub retry_after: Option<u64>,
}

impl ApiError {
    /// SnapTrade (or the broker behind it) refused the request for rate limiting.
    pub fn throttled(&self) -> bool {
        self.status == 429
    }
}

/// The error is a SnapTrade rate limit, and how long to wait if SnapTrade said.
pub fn throttle(e: &anyhow::Error) -> Option<Option<Duration>> {
    let api = e.downcast_ref::<ApiError>().filter(|a| a.throttled())?;
    Some(api.retry_after.map(Duration::from_secs))
}

/// `Retry-After` in seconds (the HTTP-date form is not used by SnapTrade).
fn retry_after_header(h: &reqwest::header::HeaderMap) -> Option<u64> {
    h.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok()
}

/// DRF's throttle text: `Request was throttled. Expected available in 37 seconds.`
fn throttle_text(body: &str) -> Option<Option<u64>> {
    let l = body.to_ascii_lowercase();
    if !l.contains("throttled") && !l.contains("rate limit") && !l.contains("too many requests") {
        return None;
    }
    let secs = l.split("available in").nth(1).and_then(|rest| {
        rest.split(|c: char| !c.is_ascii_digit() && c != '.').find(|w| !w.is_empty())?.parse::<f64>().ok()
    });
    Some(secs.map(|s| s.ceil().max(1.0) as u64))
}

/// A short, friendly line for a rate limit; never SnapTrade's raw text.
pub fn throttle_message(retry_after: Option<u64>) -> String {
    match retry_after {
        Some(s) if s > 90 => {
            format!("SnapTrade is rate-limiting requests; trying again in about {} minutes", s.div_ceil(60))
        }
        Some(s) => format!("SnapTrade is rate-limiting requests; trying again in about {s} s"),
        None => "SnapTrade is rate-limiting requests; trying again shortly".into(),
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// The request was not sent at all (for example its deadline passed while waiting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSent(pub String);

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSent {}

/// A readable message from a SnapTrade error body (`detail`, `message`, `code`), without
/// echoing anything from the request.
fn error_message(status: reqwest::StatusCode, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let detail = ["detail", "message", "error"].iter().find_map(|k| v[*k].as_str()).unwrap_or("").trim().to_string();
    let code = v["code"].as_str().or(v["code"].as_i64().map(|_| "")).unwrap_or("");
    let hint = match status.as_u16() {
        401 => " (check the client ID and consumer key)",
        403 => " (the key may not allow this, or the user secret is wrong)",
        404 => " (unknown user or account)",
        _ => "",
    };
    match (detail.is_empty(), code.is_empty()) {
        (true, _) => format!("SnapTrade returned HTTP {}{hint}", status.as_u16()),
        (false, true) => format!("SnapTrade: {detail}{hint}"),
        (false, false) => format!("SnapTrade: {detail} [{code}]{hint}"),
    }
}

fn cooldown_left() -> Option<Duration> {
    let until = (*COOLDOWN.lock().unwrap_or_else(|p| p.into_inner()))?;
    until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
}

/// Sleep until the shared cooldown has passed, re-checking after each sleep because another
/// throttled answer may have extended it meanwhile. Gives up (nothing sent) as soon as the
/// time left is longer than [`MAX_INLINE_WAIT`], or would run past `deadline`.
async fn wait_for_cooldown(deadline: Option<Instant>) -> Result<()> {
    loop {
        let Some(left) = cooldown_left() else { return Ok(()) };
        let too_late = deadline.is_some_and(|d| Instant::now() + left >= d);
        if left > MAX_INLINE_WAIT || too_late {
            let secs = left.as_secs().max(1);
            return Err(ApiError { status: 429, message: throttle_message(Some(secs)), retry_after: Some(secs) }.into());
        }
        tokio::time::sleep(left).await;
    }
}

/// Pause every request for the whole `wait` SnapTrade asked for (only the inline retry is
/// capped at [`MAX_INLINE_WAIT`], not the pause).
fn start_cooldown(wait: Duration) {
    let until = Instant::now() + wait;
    let mut c = COOLDOWN.lock().unwrap_or_else(|p| p.into_inner());
    if c.is_none_or(|t| t < until) {
        *c = Some(until);
    }
}

impl SnapTrade {
    /// Client for `SNAPTRADE_BASE_URL` (tests, proxies) or the production API.
    pub fn new(creds: Credentials) -> Result<Self> {
        let base = std::env::var("SNAPTRADE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.into());
        Self::with_base(creds, &base)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_base(creds: Credentials, base: &str) -> Result<Self> {
        if !creds.has_keys() {
            bail!("SnapTrade client ID and consumer key are not set");
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("terminal/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;
        let base = Url::parse(base.trim_end_matches('/')).context("invalid SnapTrade base URL")?;
        Ok(Self { http, base, creds })
    }

    /// SnapTrade doesn't answer cross-origin requests, so the browser build can't make a client
    /// (and so never sends an order).
    /// TODO(web proxy): call SnapTrade through a same-origin proxy that keeps the keys server-side.
    #[cfg(target_arch = "wasm32")]
    pub fn with_base(_creds: Credentials, _base: &str) -> Result<Self> {
        bail!("SnapTrade isn't available in the browser yet; use the desktop app for brokerage accounts")
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    /// Build the request URL. Query parameters are encoded once here and the exact resulting
    /// string is what gets signed and sent.
    fn url(&self, path: &str, params: &[(&str, String)], user: bool, timestamp: i64) -> Url {
        let mut url = self.base.clone();
        let prefix = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{prefix}{path}"));
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("clientId", self.creds.client_id.trim());
            q.append_pair("timestamp", &timestamp.to_string());
            if user && self.creds.has_user() {
                q.append_pair("userId", self.creds.user_id.trim());
                q.append_pair("userSecret", self.creds.user_secret.trim());
            }
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }
        url
    }

    /// One API call. At most [`MAX_CONCURRENT`] run at once; a throttled read (GET) waits as
    /// SnapTrade asks (or backs off exponentially) and is retried up to [`MAX_RETRIES`]
    /// times. Writes are never retried here: a throttled order is reported, not resent.
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
        user: bool,
    ) -> Result<Value> {
        self.call_by(method, path, params, body, user, None).await
    }

    /// [`Self::call`] that is never sent at or after `deadline` (an order preview's expiry):
    /// a rate-limit pause that would reach it fails the call unsent.
    pub(crate) async fn call_by(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
        user: bool,
        deadline: Option<Instant>,
    ) -> Result<Value> {
        let retry = method == Method::GET;
        let mut attempt = 0;
        loop {
            let result = {
                let _permit = IN_FLIGHT.acquire().await.expect("the limiter is never closed");
                // A long pause is reported rather than waited out: nothing was sent, and the
                // caller (or the user) can try again when it ends.
                wait_for_cooldown(deadline).await?;
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    anyhow::bail!(NotSent("the order's preview expired before it could be sent".into()));
                }
                self.call_once(method.clone(), path, params, body.clone(), user).await
            };
            let Err(e) = result else { return result };
            let Some(after) = throttle(&e) else { return Err(e) };
            let wait = after.unwrap_or(Duration::from_millis(1000 << attempt.min(5)));
            start_cooldown(wait);
            if !retry || attempt >= MAX_RETRIES || wait > MAX_INLINE_WAIT {
                return Err(e);
            }
            attempt += 1;
            tracing::debug!("SnapTrade throttled {path}; retry {attempt} in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    async fn call_once(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
        user: bool,
    ) -> Result<Value> {
        let url = self.url(path, params, user, chrono::Utc::now().timestamp());
        let sig = signature(self.creds.consumer_key.trim(), url.path(), url.query().unwrap_or(""), body.as_ref());
        let mut req = self.http.request(method, url).header("Signature", sig).header("Accept", "application/json");
        if let Some(b) = &body {
            // Send exactly the canonical bytes so client and server agree on the content.
            req = req.header("Content-Type", "application/json").body(canonical_json(b));
        }
        // `without_url`: the URL carries the user secret and must never reach logs or the UI.
        let resp = req.send().await.map_err(|e| anyhow!("SnapTrade request failed: {}", e.without_url()))?;
        let status = resp.status();
        let header_wait = retry_after_header(resp.headers());
        let text = resp.text().await.map_err(|e| anyhow!("reading SnapTrade response: {}", e.without_url()))?;
        if !status.is_success() {
            // Some throttles arrive with another status but DRF's "Request was throttled" text.
            let throttled =
                if status.as_u16() == 429 { Some(throttle_text(&text).flatten()) } else { throttle_text(&text) };
            return Err(match throttled {
                Some(text_wait) => {
                    let retry_after = header_wait.or(text_wait);
                    ApiError { status: 429, message: throttle_message(retry_after), retry_after }
                }
                None => ApiError { status: status.as_u16(), message: error_message(status, &text), retry_after: None },
            }
            .into());
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("SnapTrade {path}: unexpected response"))
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        self.call(Method::GET, path, params, None, true).await
    }

    /// Register a SnapTrade user for these partner keys. Returns `(userId, userSecret)`.
    pub async fn register_user(&self, user_id: &str) -> Result<(String, String)> {
        let v =
            self.call(Method::POST, "/snapTrade/registerUser", &[], Some(json!({ "userId": user_id })), false).await?;
        let id = v["userId"].as_str().unwrap_or(user_id).to_string();
        let secret = v["userSecret"].as_str().context("SnapTrade did not return a user secret")?.to_string();
        Ok((id, secret))
    }

    /// A read-only Connection Portal link (valid for five minutes) for the user to connect a
    /// brokerage in their browser, or to re-authenticate the disabled connection `reconnect`.
    pub async fn connection_portal_url(&self, dark: bool, reconnect: Option<&str>) -> Result<String> {
        let mut body = json!({ "connectionType": "read", "darkMode": dark });
        if let Some(id) = reconnect {
            body["reconnect"] = json!(id);
        }
        let v = self.call(Method::POST, "/snapTrade/login", &[], Some(body), true).await?;
        if v.get("encryptedSharedKey").is_some() {
            bail!("SnapTrade returned an encrypted login response; disable response encryption for this key");
        }
        v["redirectURI"].as_str().map(str::to_string).context("SnapTrade did not return a Connection Portal URL")
    }

    /// The user's brokerage connections.
    pub async fn connections(&self) -> Result<Vec<model::Connection>> {
        Ok(model::parse_connections(&self.get("/authorizations", &[]).await?))
    }

    /// Remove a brokerage connection and its accounts from SnapTrade (irreversible; the user
    /// can connect the broker again through the portal).
    pub async fn delete_connection(&self, id: &str) -> Result<()> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("invalid connection id");
        }
        self.call(Method::DELETE, &format!("/connection/{id}"), &[], None, true).await.map(|_| ())
    }

    pub async fn accounts(&self) -> Result<Value> {
        self.get("/accounts", &[]).await
    }

    pub async fn balances(&self, account: &str) -> Result<Value> {
        self.get(&format!("/accounts/{account}/balances"), &[]).await
    }

    pub async fn positions(&self, account: &str) -> Result<Value> {
        self.get(&format!("/accounts/{account}/positions/all"), &[]).await
    }

    pub async fn orders(&self, account: &str) -> Result<Value> {
        self.get(&format!("/accounts/{account}/orders"), &[("state", "all".into()), ("days", ORDER_DAYS.to_string())])
            .await
    }

    pub async fn activities(&self, account: &str) -> Result<Value> {
        let end = chrono::Utc::now().date_naive();
        let start = end - chrono::Duration::days(ACTIVITY_DAYS);
        self.get(
            &format!("/accounts/{account}/activities"),
            &[
                ("startDate", start.format("%Y-%m-%d").to_string()),
                ("endDate", end.format("%Y-%m-%d").to_string()),
                ("limit", ACTIVITY_LIMIT.to_string()),
            ],
        )
        .await
    }

    /// Accounts with balances, positions, recent orders and activity. A failure on one
    /// account's sub-request becomes a warning; failing to list accounts is an error.
    pub async fn portfolio(&self) -> Result<Portfolio> {
        let (accounts, connections) = futures::join!(self.accounts(), self.connections());
        let mut accounts = model::parse_accounts(&accounts?);
        let per_account = futures::future::join_all(accounts.iter().map(|a| async move {
            let id = a.id.as_str();
            futures::join!(self.balances(id), self.positions(id), self.orders(id), self.activities(id))
        }))
        .await;
        let mut pf = Portfolio { fetched: Some(chrono::Utc::now()), ..Default::default() };
        match connections {
            Ok(c) => pf.connections = c,
            Err(e) => pf.warnings.push(format!("connections: {e:#}")),
        }
        for (acct, (bal, pos, ord, act)) in accounts.iter_mut().zip(per_account) {
            let label = acct.label();
            let mut warn = |what: &str, e: anyhow::Error| pf.warnings.push(format!("{label}: {what}: {e:#}"));
            match bal {
                Ok(v) => model::apply_balances(acct, &v),
                Err(e) => warn("balances", e),
            }
            match pos {
                Ok(v) => pf.positions.extend(model::parse_positions(&acct.id, &v)),
                Err(e) => warn("positions", e),
            }
            match ord {
                Ok(v) => pf.orders.extend(model::parse_orders(&acct.id, &v)),
                Err(e) => warn("orders", e),
            }
            match act {
                Ok(v) => pf.activities.extend(model::parse_activities(&acct.id, &v)),
                Err(e) => warn("activity", e),
            }
        }
        pf.orders.sort_by_key(|o| std::cmp::Reverse(o.placed));
        pf.activities.sort_by_key(|a| std::cmp::Reverse(a.trade_date));
        pf.accounts = accounts;
        Ok(pf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_and_compacts() {
        let v = json!({"query": "a=1", "content": {"b": [1, 2.5, null], "a": true}, "path": "/x"});
        assert_eq!(canonical_json(&v), r#"{"content":{"a":true,"b":[1,2.5,null]},"path":"/x","query":"a=1"}"#);
        assert_eq!(canonical_json(&json!("\u{e9}\"\n\u{7f}")), r#""\u00e9\"\n\u007f""#);
        assert_eq!(canonical_json(&json!("\u{1f600}")), r#""\ud83d\ude00""#);
    }

    #[test]
    fn signature_matches_the_documented_scheme() {
        // The documented example payload, signed with a known key (value computed with
        // Python's hmac/hashlib over the canonical string from the SnapTrade docs).
        let body = json!({"substring": "AAPL"});
        let sig = signature(
            "YOUR_CONSUMER_KEY",
            "/api/v1/symbols",
            "clientId=YOUR_CLIENT_ID&timestamp=1715123456",
            Some(&body),
        );
        assert_eq!(sig, "XURYf7Z+QeT+M7Y4OPdK/NaYhDQsds4cEKunvbtu5sE=");
        // Bodyless requests and empty bodies both sign `"content":null`.
        let a = signature("consumer_key", "/accounts", "timestamp=1700000000", None);
        let b = signature("consumer_key", "/accounts", "timestamp=1700000000", Some(&json!({})));
        assert_eq!(a, b);
        assert_eq!(a, "3VkE1AkN6sr1yxWbl8JA7v9O9MFXfMFDVgzhC9GhptU=");
    }

    #[test]
    fn url_carries_auth_and_params_in_signing_order() {
        let creds = Credentials {
            client_id: "CID".into(),
            consumer_key: "k".into(),
            user_id: "u 1".into(),
            user_secret: "s/2".into(),
        };
        let c = SnapTrade::with_base(creds.clone(), "https://api.example.test").unwrap();
        let u = c.url("/accounts/x/orders", &[("state", "all".into())], true, 1_700_000_000);
        assert_eq!(u.path(), "/accounts/x/orders");
        assert_eq!(u.query(), Some("clientId=CID&timestamp=1700000000&userId=u+1&userSecret=s%2F2&state=all"));
        let partner = c.url("/snapTrade/registerUser", &[], false, 1);
        assert_eq!(partner.query(), Some("clientId=CID&timestamp=1"));
        let personal = Credentials { user_id: String::new(), user_secret: String::new(), ..creds };
        let p = SnapTrade::with_base(personal, "https://api.example.test/").unwrap();
        assert_eq!(
            p.url("/accounts", &[], true, 1).query(),
            Some("clientId=CID&timestamp=1"),
            "personal keys send no user"
        );
        assert!(SnapTrade::with_base(Credentials::default(), DEFAULT_BASE).is_err());
    }

    #[test]
    fn error_messages_are_readable() {
        let m = error_message(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"detail":"Unable to verify signature sent","status_code":401,"code":"1076"}"#,
        );
        assert_eq!(m, "SnapTrade: Unable to verify signature sent [1076] (check the client ID and consumer key)");
        assert_eq!(error_message(reqwest::StatusCode::BAD_GATEWAY, "<html>"), "SnapTrade returned HTTP 502");
    }

    #[test]
    fn throttle_messages_are_parsed_and_friendly() {
        assert_eq!(
            throttle_text(r#"{"detail":"Request was throttled. Expected available in 37 seconds."}"#),
            Some(Some(37))
        );
        assert_eq!(throttle_text(r#"{"detail":"Request was throttled."}"#), Some(None));
        assert_eq!(throttle_text(r#"{"detail":"Unknown symbol"}"#), None);
        assert_eq!(throttle_message(Some(30)), "SnapTrade is rate-limiting requests; trying again in about 30 s");
        assert!(throttle_message(Some(600)).contains("10 minutes"));
        let e: anyhow::Error = ApiError { status: 429, message: throttle_message(None), retry_after: Some(5) }.into();
        assert_eq!(throttle(&e), Some(Some(Duration::from_secs(5))));
        assert_eq!(throttle(&anyhow!("other")), None);
    }
}
