//! SnapTrade rate limits: throttled reads wait as asked and retry, throttled orders are never
//! resent, and the error the user sees is a friendly line rather than SnapTrade's raw text.
//! (Its own test binary: the client's cooldown is process-wide.)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tr_broker::trading::{Amount, OrderPreview, OrderRequest, OrderType, PlaceError, Side, SymbolMatch, TimeInForce};
use tr_broker::{Credentials, SnapTrade};

const THROTTLED: &str = r#"{"detail":"Request was throttled. Expected available in 1 seconds."}"#;

/// Answers `/authorizations` with a 429 the first `throttle_reads` times, every
/// `/trade/...` with a 429, and records each request line.
async fn mock(throttle_reads: usize, seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let (seen, reads) = (seen.clone(), reads.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                let path = line.split(' ').nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
                let method = line.split(' ').next().unwrap_or("").to_string();
                seen.lock().unwrap().push(format!("{method} {path}"));
                let (status, extra, body) = if path == "/authorizations" {
                    if reads.fetch_add(1, Ordering::SeqCst) < throttle_reads {
                        (429, "Retry-After: 1\r\n", THROTTLED.to_string())
                    } else {
                        (200, "", r#"[{"id":"c1","type":"trade","brokerage":{"display_name":"Wealthsimple","allows_trading":true}}]"#.to_string())
                    }
                } else if path.starts_with("/trade/") {
                    (429, "", THROTTLED.to_string())
                } else {
                    (404, "", r#"{"detail":"Not found"}"#.to_string())
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
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
        client_id: "TEST".into(),
        consumer_key: "key".into(),
        user_id: "terminal-test".into(),
        user_secret: "s".into(),
    };
    SnapTrade::with_base(creds, base).unwrap()
}

#[tokio::test]
async fn throttled_reads_retry_and_orders_are_never_resent() {
    // A read throttled once waits for Retry-After and succeeds.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = mock(1, seen.clone()).await;
    let start = Instant::now();
    let caps = client(&base).connection_capabilities().await.unwrap();
    assert!(caps["c1"].can_trade());
    assert!(start.elapsed().as_millis() >= 900, "waited for Retry-After");
    assert_eq!(seen.lock().unwrap().iter().filter(|l| l.ends_with("/authorizations")).count(), 2);

    // A read that stays throttled gives up with a friendly message.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = mock(usize::MAX, seen.clone()).await;
    let e = client(&base).connection_capabilities().await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(tr_broker::snaptrade::throttle(&e).is_some());
    assert!(msg.contains("rate-limiting") && !msg.contains("Request was throttled"), "{msg}");
    assert_eq!(seen.lock().unwrap().len(), 1 + tr_broker::snaptrade::MAX_RETRIES as usize);

    // A throttled placement is sent exactly once and reported as not placed.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = mock(0, seen.clone()).await;
    let req = OrderRequest {
        account_id: "acct-1".into(),
        symbol: "AAPL".into(),
        side: Side::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Day,
        amount: Amount::Units(1.0),
        limit: None,
        stop: None,
    };
    let preview = OrderPreview {
        trade_id: "139e307a-82f7-4402-b39e-4da7baa87758".into(),
        request: req,
        symbol: SymbolMatch {
            id: "s".into(),
            symbol: "AAPL".into(),
            description: String::new(),
            currency: "USD".into(),
            exchange: String::new(),
        },
        price: None,
        units: None,
        commission: None,
        forex_fees: None,
        remaining_cash: None,
        currency: "USD".into(),
        created: chrono::Utc::now(),
        simulated: false,
    };
    let err = client(&base).place_previewed(&preview).await.unwrap_err();
    assert!(matches!(err, PlaceError::Rejected(_)), "{err:?}");
    assert!(err.message().contains("rate-limiting"));
    assert_eq!(seen.lock().unwrap().iter().filter(|l| l.starts_with("POST /trade/")).count(), 1);

    // A pause that would outlast the preview never sends the order late.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = mock_retry_after(4, seen.clone()).await;
    let _ = client(&base).connection_capabilities().await; // starts a 4 s pause
    let mut soon = preview.clone();
    soon.created = chrono::Utc::now()
        - chrono::Duration::seconds(tr_broker::trading::PREVIEW_TTL_SECS - tr_broker::trading::PREVIEW_MARGIN_SECS - 2);
    let start = Instant::now();
    let e = client(&base).place_previewed(&soon).await.unwrap_err();
    assert!(start.elapsed().as_secs() < 2, "refused at once, not after the pause");
    assert!(!e.uncertain(), "{e:?}");
    assert!(seen.lock().unwrap().iter().all(|l| !l.starts_with("POST /trade/")), "the stale order was never sent");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    // A long Retry-After pauses every request for its whole length, and nothing is sent
    // meanwhile: the call fails at once, unsent, with the time left.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = mock_retry_after(120, seen.clone()).await;
    let e = client(&base).connection_capabilities().await.unwrap_err();
    assert!(tr_broker::snaptrade::throttle(&e).flatten().is_some_and(|d| d.as_secs() >= 100));
    let start = Instant::now();
    let e = client(&base).place_previewed(&preview).await.unwrap_err();
    assert!(start.elapsed().as_secs() < 5, "an order is never held for minutes");
    assert!(matches!(e, PlaceError::Rejected(_)) && e.message().contains("minutes"), "{e:?}");
    assert_eq!(seen.lock().unwrap().len(), 1, "nothing sent during the pause");
}

/// Answers everything with a 429 asking to wait `secs`.
async fn mock_retry_after(secs: u64, seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.lock().unwrap().push(head.lines().next().unwrap_or("").to_string());
                let body = r#"{"detail":"Request was throttled."}"#;
                let reply = format!(
                    "HTTP/1.1 429 X\r\nRetry-After: {secs}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}
