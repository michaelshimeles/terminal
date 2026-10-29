//! End-to-end client test against a local mock of the SnapTrade API: every request's
//! signature, timestamp and auth parameters are verified the way the server does, and a full
//! portfolio is loaded from captured-shape fixtures.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tr_broker::snaptrade::signature;
use tr_broker::{Credentials, InstrumentKind, SnapTrade};

const CONSUMER_KEY: &str = "test-consumer-key-0123456789";
const ACCT_A: &str = "917c8734-8470-4a3e-a18f-57c3f2ee6631";
const ACCT_B: &str = "2bcd7cc3-e922-4976-bce1-9858296801c3";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    body: Option<Value>,
}

/// Parse one HTTP/1.1 request: method, target, headers and body.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((method, target, headers, body))
}

/// Mock SnapTrade: verifies auth on every request, then routes to fixtures.
async fn mock(seen: Arc<Mutex<Vec<Seen>>>, failures: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let seen = seen.clone();
            let failures = failures.clone();
            tokio::spawn(async move {
                let Some((method, target, headers, body)) = read_request(&mut sock).await else { return };
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let body_json: Option<Value> = (!body.is_empty()).then(|| serde_json::from_slice(&body).unwrap());
                let sig = headers.iter().find(|(k, _)| k == "signature").map(|(_, v)| v.clone()).unwrap_or_default();
                let expected = signature(CONSUMER_KEY, path, query, body_json.as_ref());
                let params: Vec<(String, String)> = url_pairs(query);
                let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                let fail = |m: String| failures.lock().unwrap().push(format!("{method} {path}: {m}"));
                if sig != expected {
                    fail(format!("bad signature {sig} (expected {expected})"));
                }
                if param("clientId").as_deref() != Some("TEST-CLIENT") {
                    fail("missing clientId".into());
                }
                let ts: i64 = param("timestamp").and_then(|t| t.parse().ok()).unwrap_or(0);
                if (chrono::Utc::now().timestamp() - ts).abs() > 60 {
                    fail(format!("stale timestamp {ts}"));
                }
                let user_scoped = path.starts_with("/accounts")
                    || path.starts_with("/authorizations")
                    || path.starts_with("/connection/")
                    || path == "/snapTrade/login";
                if user_scoped
                    && (param("userId").as_deref() != Some("terminal-test")
                        || param("userSecret").as_deref() != Some("s3cr3t/+="))
                {
                    fail("missing or wrong user credentials".into());
                }
                if path == "/snapTrade/registerUser" && param("userSecret").is_some() {
                    fail("partner request leaked the user secret".into());
                }
                seen.lock().unwrap().push(Seen {
                    method: method.clone(),
                    path: path.to_string(),
                    query: query.to_string(),
                    body: body_json.clone(),
                });

                let (status, out) = match (method.as_str(), path) {
                    ("POST", "/snapTrade/registerUser") => {
                        let id = body_json.as_ref().and_then(|b| b["userId"].as_str()).unwrap_or("?").to_string();
                        (200, serde_json::json!({"userId": id, "userSecret": "fresh-secret"}).to_string())
                    }
                    ("POST", "/snapTrade/login") => {
                        (200, r#"{"redirectURI":"https://app.snaptrade.com/snapTrade/redeemToken?token=abc","sessionId":"s1"}"#.into())
                    }
                    ("GET", "/accounts") => (200, fixture("accounts.json")),
                    ("GET", "/authorizations") => (200, fixture("connections.json")),
                    ("DELETE", p) if p.starts_with("/connection/") => {
                        (200, r#"{"detail":"Connection deletion queued","status":"queued"}"#.into())
                    }
                    ("GET", p) if p == format!("/accounts/{ACCT_B}/positions/all") => {
                        (403, r#"{"detail":"Holdings are unavailable for this account","status_code":403,"code":"1063"}"#.into())
                    }
                    ("GET", p) if p.ends_with("/balances") => (200, fixture("balances.json")),
                    ("GET", p) if p.ends_with("/positions/all") => (200, fixture("positions.json")),
                    ("GET", p) if p == format!("/accounts/{ACCT_A}/orders") => (200, fixture("orders.json")),
                    ("GET", p) if p.ends_with("/orders") => (200, "[]".into()),
                    ("GET", p) if p == format!("/accounts/{ACCT_A}/activities") => (200, fixture("activities.json")),
                    ("GET", p) if p.ends_with("/activities") => (200, r#"{"data":[],"pagination":{"offset":0,"limit":100,"total":0}}"#.into()),
                    _ => (404, r#"{"detail":"Not found"}"#.into()),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn url_pairs(query: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://x/?{query}"))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn creds() -> Credentials {
    Credentials {
        client_id: "TEST-CLIENT".into(),
        consumer_key: CONSUMER_KEY.into(),
        user_id: "terminal-test".into(),
        user_secret: "s3cr3t/+=".into(),
    }
}

#[tokio::test]
async fn loads_a_signed_portfolio() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let base = mock(seen.clone(), failures.clone()).await;
    let client = SnapTrade::with_base(creds(), &base).unwrap();

    let pf = client.portfolio().await.unwrap();
    assert_eq!(*failures.lock().unwrap(), Vec::<String>::new());

    // Accounts with balances matched by currency.
    assert_eq!(pf.accounts.len(), 2);
    let a = pf.account(ACCT_A).unwrap();
    assert_eq!(a.label(), "Individual Margin");
    assert_eq!(a.total, Some(31884.62));
    assert_eq!(a.cash, Some(1520.71), "USD row chosen for a USD account");
    assert_eq!(a.buying_power, Some(3041.42), "string decimals parse");
    assert!(a.synced.is_some());
    let b = pf.account(ACCT_B).unwrap();
    assert_eq!(b.label(), "Questrade ••••77Q2", "unnamed accounts use institution and masked number");
    assert_eq!(b.total, None);

    // Connections, linked to their accounts.
    assert_eq!(pf.connections.len(), 2);
    let rh = &pf.connections[0];
    assert_eq!((rh.brokerage.as_str(), rh.disabled, rh.trading), ("Robinhood", false, false));
    assert_eq!(pf.accounts_of(&rh.id).map(|a| a.id.as_str()).collect::<Vec<_>>(), [ACCT_A]);
    assert!(pf.connections[1].disabled && pf.connections[1].trading);

    // One account's positions failed: a warning, not an error.
    assert_eq!(pf.warnings.len(), 1, "{:?}", pf.warnings);
    assert!(pf.warnings[0].contains("Holdings are unavailable"), "{}", pf.warnings[0]);

    // Positions: every instrument kind, blank symbols dropped.
    let syms: Vec<&str> = pf.positions.iter().map(|p| p.symbol.as_str()).collect();
    assert_eq!(syms, ["AAPL", "SPY", "BTC", "NVDA  261218C00250000", "SWVXX"]);
    let aapl = &pf.positions[0];
    assert_eq!((aapl.units, aapl.price, aapl.cost_basis), (50.0, Some(339.51), Some(190.25)));
    assert_eq!(aapl.market_key().as_deref(), Some("AAPL US Equity"));
    let btc = &pf.positions[2];
    assert_eq!(btc.kind, InstrumentKind::Crypto);
    assert_eq!(btc.market_key().as_deref(), Some("BTCUSD Curncy"));
    assert!(btc.pnl_at(None).unwrap() < 0.0);
    let call = &pf.positions[3];
    assert_eq!(call.kind, InstrumentKind::Option);
    assert_eq!(call.multiplier, 100.0);
    assert_eq!(call.description, "NVDA 18 Dec 26 250 C");
    assert_eq!(call.market_key().as_deref(), Some("NVDA US Equity"));
    assert!((call.pnl_at(None).unwrap() - 175.0).abs() < 1e-9, "short call gained (8.15 - 6.40) x 100");
    assert!(pf.positions[4].cash_equivalent);

    // Orders newest first, options labelled by underlying.
    assert_eq!(pf.orders.len(), 2);
    assert_eq!(pf.orders[0].status, "PENDING");
    assert!(pf.orders[0].is_open());
    assert_eq!(pf.orders[0].description, "NVDA 18 Dec 26 250 C");
    assert_eq!(pf.orders[0].market_key().as_deref(), Some("NVDA US Equity"));
    assert_eq!(pf.orders[1].execution_price, Some(331.2));
    assert!(pf.orders[1].is_buy() && !pf.orders[1].is_open());

    // Activity newest first.
    assert_eq!(pf.activities.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(), ["BUY", "DIVIDEND"]);
    assert_eq!(pf.activities[0].amount, Some(-3312.0));
    assert_eq!(pf.activities[1].account_id, ACCT_A, "falls back to the requested account");

    // Query parameters the API expects.
    let seen = seen.lock().unwrap().clone();
    let orders = seen.iter().find(|s| s.path == format!("/accounts/{ACCT_A}/orders")).unwrap();
    assert!(orders.query.contains("state=all") && orders.query.contains("days=30"), "{}", orders.query);
    let acts = seen.iter().find(|s| s.path == format!("/accounts/{ACCT_A}/activities")).unwrap();
    assert!(acts.query.contains("startDate=") && acts.query.contains("limit=100"), "{}", acts.query);
    assert!(seen.iter().all(|s| s.method == "GET"), "loading a portfolio only reads");
}

#[tokio::test]
async fn registers_a_user_opens_the_portal_and_disconnects() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let base = mock(seen.clone(), failures.clone()).await;

    let partner =
        SnapTrade::with_base(Credentials { user_id: String::new(), user_secret: String::new(), ..creds() }, &base)
            .unwrap();
    let (id, secret) = partner.register_user("terminal-test").await.unwrap();
    assert_eq!((id.as_str(), secret.as_str()), ("terminal-test", "fresh-secret"));

    let client = SnapTrade::with_base(creds(), &base).unwrap();
    let url = client.connection_portal_url(true, None).await.unwrap();
    client.connection_portal_url(false, Some("c9d1a0f2-0c9e-4a51-8b7e-5d1c2f0a9b11")).await.unwrap();
    client.delete_connection("c9d1a0f2-0c9e-4a51-8b7e-5d1c2f0a9b11").await.unwrap();
    assert!(client.delete_connection("../accounts").await.is_err(), "ids are validated before use in a path");
    assert!(url.starts_with("https://app.snaptrade.com/"));
    assert_eq!(*failures.lock().unwrap(), Vec::<String>::new());

    let seen = seen.lock().unwrap().clone();
    let logins: Vec<_> = seen.iter().filter(|s| s.path == "/snapTrade/login").collect();
    assert_eq!(logins[0].body.as_ref().unwrap()["connectionType"], "read", "the portal asks for read-only access");
    assert!(logins[0].body.as_ref().unwrap().get("reconnect").is_none());
    assert_eq!(logins[1].body.as_ref().unwrap()["reconnect"], "c9d1a0f2-0c9e-4a51-8b7e-5d1c2f0a9b11");
    let del = seen.iter().find(|s| s.method == "DELETE").unwrap();
    assert_eq!(del.path, "/connection/c9d1a0f2-0c9e-4a51-8b7e-5d1c2f0a9b11");
    assert!(del.body.is_none(), "a bodyless DELETE signs content as null");
}

#[tokio::test]
async fn errors_never_reveal_the_user_secret() {
    // Nothing listens here: the transport error must not echo the URL (which holds the secret).
    let client = SnapTrade::with_base(creds(), "http://127.0.0.1:9").unwrap();
    let err = format!("{:#}", client.accounts().await.unwrap_err());
    assert!(!err.contains("s3cr3t") && !err.contains("userSecret"), "{err}");
}
