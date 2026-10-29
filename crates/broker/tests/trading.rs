//! Order entry against a local mock of the SnapTrade trading API. No test here talks to
//! SnapTrade or a broker: every client points at `127.0.0.1`, and the mock checks each
//! request's signature and user credentials before answering with captured-shape responses.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tr_broker::snaptrade::signature;
use tr_broker::trading::{Amount, OrderRequest, OrderType, Side, TimeInForce};
use tr_broker::{Credentials, SnapTrade};

const CONSUMER_KEY: &str = "trading-test-consumer-key";
const ACCT: &str = "917c8734-8470-4a3e-a18f-57c3f2ee6631";
const TRADE_ID: &str = "139e307a-82f7-4402-b39e-4da7baa87758";
const AAPL_ID: &str = "2bcd7cc3-e922-4976-bce1-9858296801c3";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: Option<Value>,
}

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

fn param(query: &str, k: &str) -> Option<String> {
    reqwest::Url::parse(&format!("http://x/?{query}"))
        .unwrap()
        .query_pairs()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.into_owned())
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Mock trading API: symbol search, order impact, place checked order, cancel.
async fn mock(seen: Log, failures: Log) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let (seen, failures) = (seen.clone(), failures.clone());
            tokio::spawn(async move {
                let Some((method, target, headers, body)) = read_request(&mut sock).await else { return };
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let body: Option<Value> = (!body.is_empty()).then(|| serde_json::from_slice(&body).unwrap());
                let sig = headers.iter().find(|(k, _)| k == "signature").map(|(_, v)| v.clone()).unwrap_or_default();
                let ok = sig == signature(CONSUMER_KEY, path, query, body.as_ref())
                    && param(query, "userId").as_deref() == Some("terminal-test")
                    && param(query, "userSecret").as_deref() == Some("s3cr3t");
                let entry = Seen { method: method.clone(), path: path.to_string(), body: body.clone() };
                if !ok {
                    failures.lock().unwrap().push(entry.clone());
                }
                seen.lock().unwrap().push(entry);
                let b = body.unwrap_or(Value::Null);
                if path == "/trade/dropped-trade" {
                    // The broker got the order but the answer never arrives.
                    let _ = sock.shutdown().await;
                    return;
                }
                let (status, out) = match (method.as_str(), path) {
                    ("POST", p) if p == format!("/accounts/{ACCT}/symbols") => (
                        200,
                        json!([
                            {"id": "x1", "symbol": "AAPL.NE", "currency": {"code": "CAD"}, "exchange": {"code": "NEO"}},
                            {"id": AAPL_ID, "symbol": "AAPL", "raw_symbol": "AAPL", "description": "Apple Inc.",
                             "currency": {"code": "USD"}, "exchange": {"code": "XNAS", "name": "NASDAQ"}}
                        ]),
                    ),
                    ("POST", "/trade/impact") if b["universal_symbol_id"] == AAPL_ID => (
                        200,
                        json!({
                            "trade": {"id": TRADE_ID, "account": ACCT, "order_type": b["order_type"],
                                      "time_in_force": b["time_in_force"], "action": b["action"],
                                      "units": b["units"], "price": 187.25,
                                      "symbol": {"universal_symbol_id": AAPL_ID, "currency": {"code": "USD"}}},
                            "trade_impacts": [{"account": ACCT, "currency": "usd-id", "remaining_cash": 6548.05,
                                               "estimated_commission": 0.0, "forex_fees": 0.0}],
                            "combined_remaining_balance": {"currency": {"code": "USD"}, "cash": 6548.05}
                        }),
                    ),
                    ("POST", "/trade/impact") => (400, json!({"detail": "Unknown symbol", "code": "1019"})),
                    ("POST", "/trade/rejected-trade") => {
                        (400, json!({"detail": "Insufficient buying power", "code": "1068"}))
                    }
                    ("POST", "/trade/flaky-trade") => (503, json!({"detail": "Upstream timeout"})),
                    ("POST", p) if p == format!("/trade/{TRADE_ID}") => (
                        200,
                        json!({
                            "brokerage_order_id": "66844", "status": "PENDING",
                            "universal_symbol": {"symbol": "AAPL", "description": "Apple Inc.", "type": {"code": "cs"}},
                            "action": "BUY", "total_quantity": "10", "filled_quantity": "0",
                            "order_type": "Limit", "time_in_force": "Day", "limit_price": "187.25",
                            "time_placed": "2026-10-12T14:31:02.000Z"
                        }),
                    ),
                    ("POST", p) if p == format!("/accounts/{ACCT}/trading/cancel") => {
                        (200, json!({"brokerage_order_id": b["brokerage_order_id"], "raw_response": null}))
                    }
                    ("GET", "/accounts") => (200, json!([{"id": ACCT, "brokerage_authorization": "c1"}])),
                    ("GET", "/authorizations") => (
                        200,
                        json!([{"id": "c1", "type": "trade", "disabled": false,
                                "brokerage": {"slug": "WEALTHSIMPLETRADE", "display_name": "Wealthsimple",
                                              "allows_trading": true, "allows_fractional_units": true}}]),
                    ),
                    ("POST", "/snapTrade/login") => {
                        (200, json!({"redirectURI": "https://app.snaptrade.com/snapTrade/redeemToken?token=t"}))
                    }
                    _ => (404, json!({"detail": "Not found"})),
                };
                let out = out.to_string();
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

fn client(base: &str) -> SnapTrade {
    let creds = Credentials {
        client_id: "TEST-CLIENT".into(),
        consumer_key: CONSUMER_KEY.into(),
        user_id: "terminal-test".into(),
        user_secret: "s3cr3t".into(),
    };
    SnapTrade::with_base(creds, base).unwrap()
}

fn limit_buy() -> OrderRequest {
    OrderRequest {
        account_id: ACCT.into(),
        symbol: "aapl".into(),
        side: Side::Buy,
        order_type: OrderType::Limit,
        time_in_force: TimeInForce::Day,
        amount: Amount::Units(10.0),
        limit: Some(187.25),
        stop: None,
    }
}

#[tokio::test]
async fn previews_places_and_cancels_against_the_mock() {
    let (seen, failures): (Log, Log) = Default::default();
    let base = mock(seen.clone(), failures.clone()).await;
    let c = client(&base);

    let preview = c.order_impact(&limit_buy()).await.unwrap();
    assert_eq!(preview.trade_id, TRADE_ID);
    assert_eq!(preview.symbol.id, AAPL_ID, "the exact ticker wins over AAPL.NE");
    assert_eq!(preview.remaining_cash, Some(6548.05));
    assert_eq!(preview.estimated_total(None), Some(1872.5));
    assert!(!preview.simulated);

    let placed = c.place_previewed(&preview).await.unwrap().unwrap();
    assert_eq!((placed.id.as_str(), placed.status.as_str()), ("66844", "PENDING"));
    assert_eq!((placed.quantity, placed.limit_price), (Some(10.0), Some(187.25)));

    c.cancel_order(ACCT, "66844").await.unwrap();
    let url = c.trading_portal_url(true).await.unwrap();
    assert!(url.starts_with("https://app.snaptrade.com/"));

    assert!(failures.lock().unwrap().is_empty(), "unsigned or unauthenticated: {:?}", failures.lock().unwrap());
    let seen = seen.lock().unwrap().clone();
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            format!("/accounts/{ACCT}/symbols").as_str(),
            "/trade/impact",
            format!("/trade/{TRADE_ID}").as_str(),
            format!("/accounts/{ACCT}/trading/cancel").as_str(),
            "/snapTrade/login",
        ]
    );
    assert!(seen.iter().all(|s| s.method == "POST"));
    assert_eq!(seen[0].body.as_ref().unwrap()["substring"], "AAPL");
    let impact = seen[1].body.as_ref().unwrap();
    assert_eq!(impact["account_id"], ACCT);
    assert_eq!((impact["action"].as_str(), impact["order_type"].as_str()), (Some("BUY"), Some("Limit")));
    assert_eq!((impact["price"].as_f64(), impact["units"].as_f64()), (Some(187.25), Some(10.0)));
    assert_eq!(seen[2].body.as_ref().unwrap()["wait_to_confirm"], true);
    assert_eq!(seen[3].body.as_ref().unwrap()["brokerage_order_id"], "66844");
    assert_eq!(seen[4].body.as_ref().unwrap()["connectionType"], "trade-if-available");
}

#[tokio::test]
async fn reads_trading_capabilities() {
    let (seen, failures): (Log, Log) = Default::default();
    let base = mock(seen.clone(), failures.clone()).await;
    let caps = client(&base).capabilities().await.unwrap();
    assert!(caps[ACCT].can_trade() && caps[ACCT].fractional);
    assert_eq!(caps[ACCT].brokerage, "Wealthsimple");
    assert!(failures.lock().unwrap().is_empty());
    assert!(seen.lock().unwrap().iter().all(|s| s.method == "GET"), "reading capabilities sends no orders");
}

#[tokio::test]
async fn placement_errors_separate_rejections_from_unknown_outcomes() {
    use tr_broker::trading::PlaceError;
    let (seen, failures): (Log, Log) = Default::default();
    let base = mock(seen, failures).await;
    let c = client(&base);
    let preview = c.order_impact(&limit_buy()).await.unwrap();
    let with_id = |id: &str| {
        let mut p = preview.clone();
        p.trade_id = id.into();
        p
    };
    match c.place_previewed(&with_id("rejected-trade")).await {
        Err(PlaceError::Rejected(m)) => assert!(m.contains("Insufficient buying power"), "{m}"),
        other => panic!("a 4xx is a definite rejection: {other:?}"),
    }
    let flaky = c.place_previewed(&with_id("flaky-trade")).await.unwrap_err();
    assert!(flaky.uncertain(), "a 5xx may hide an accepted order: {flaky:?}");
    let dropped = c.place_previewed(&with_id("dropped-trade")).await.unwrap_err();
    assert!(dropped.uncertain(), "a lost response may hide an accepted order: {dropped:?}");
    let mut old = preview.clone();
    old.created -= chrono::Duration::seconds(tr_broker::trading::PREVIEW_TTL_SECS);
    assert!(matches!(c.place_previewed(&old).await, Err(PlaceError::NotSent(_))));
}

#[tokio::test]
async fn refuses_bad_input_before_sending_anything() {
    let (seen, failures): (Log, Log) = Default::default();
    let base = mock(seen.clone(), failures).await;
    let c = client(&base);

    let mut bad = limit_buy();
    bad.limit = None;
    assert!(c.order_impact(&bad).await.unwrap_err().to_string().contains("limit price"));
    assert!(c.cancel_order("../accounts", "1").await.is_err());
    assert!(c.cancel_order(ACCT, "1/../../x").await.is_err());

    // An expired preview is never sent.
    let mut preview = c.order_impact(&limit_buy()).await.unwrap();
    preview.created -= chrono::Duration::seconds(tr_broker::trading::PREVIEW_TTL_SECS);
    assert!(c.place_previewed(&preview).await.unwrap_err().to_string().contains("expired"));
    // Neither is a simulated one.
    let mut sim = c.order_impact(&limit_buy()).await.unwrap();
    sim.simulated = true;
    assert!(c.place_previewed(&sim).await.is_err());

    // Unknown tickers fail at the symbol lookup with a suggestion.
    let mut other = limit_buy();
    other.symbol = "AAP".into();
    let e = c.order_impact(&other).await.unwrap_err().to_string();
    assert!(e.contains("did you mean AAPL.NE, AAPL"), "{e}");

    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|s| !s.path.starts_with("/trade/1") && !s.path.contains("..")));
    assert!(!seen.iter().any(|s| s.path == format!("/trade/{TRADE_ID}")), "nothing was placed");
}
