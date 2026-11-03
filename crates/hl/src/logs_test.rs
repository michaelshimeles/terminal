//! A whole place-and-cancel cycle against a local mock `/exchange`, with every `tracing`
//! line captured: nothing in the logs may look like a private key, a signature or a signed
//! payload (any run of 64 or more hex digits).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::assets::fixtures::registry;
use crate::client::{Client, Status};
use crate::key::AgentKey;
use crate::nonce::NonceManager;
use crate::trader::*;

/// Answers `/exchange` like Hyperliquid: a resting order, then a successful cancel.
async fn mock_exchange() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read headers and the body (Content-Length).
                loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf);
                let body = if req.contains(r#""type":"cancel""#) {
                    r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#
                } else {
                    r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":4242}}]}}}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn longest_hex_run(s: &str) -> usize {
    s.split(|c: char| !c.is_ascii_hexdigit()).map(str::len).max().unwrap_or(0)
}

#[tokio::test]
async fn a_place_and_cancel_cycle_logs_no_secrets() {
    let cap = Capture::default();
    let writer = cap.clone();
    let sub =
        tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(move || writer.clone()).finish();
    let _guard = tracing::subscriber::set_default(sub);

    let base = mock_exchange().await;
    let client = Client::mock(&base).unwrap();
    let reg = registry();
    let key = AgentKey::generate();
    let nonces = NonceManager::new();
    let ctx = MarketCtx { mid: Some(76_662.5), leverage: Some(Leverage { value: 3, cross: true }) };
    let intent = OrderIntent {
        coin: "BTC".into(),
        is_buy: true,
        size: 0.0002,
        kind: OrderKind::Limit { px: 50_000.0, tif: crate::actions::Tif::Gtc },
        reduce_only: false,
    };
    let plan = plan_order(&reg, &intent, &ctx, &crate::actions::cloid(7)).unwrap();
    let signed = sign_order(&key, &nonces, &plan, &ctx, crate::nonce::now_ms()).unwrap();
    // Whatever a careless caller might log, too.
    tracing::info!("key {key:?} / {key} / signed {signed:?} / plan cloid {}", plan.cloid());
    let placed = client.send(&signed).await.unwrap();
    assert_eq!(placed.statuses, [Status::Resting { oid: 4242 }]);
    let cancel = sign_cancel(&key, &nonces, reg.resolve("BTC").unwrap(), 4242, crate::nonce::now_ms()).unwrap();
    let cancelled = client.send(&cancel).await.unwrap();
    assert_eq!(cancelled.statuses, [Status::Success]);

    let logs = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("resting oid 4242") && logs.contains("success"), "the cycle was logged:\n{logs}");
    let secret = key.to_hex();
    assert!(!logs.contains(&secret[2..]), "the private key is in the logs");
    assert!(
        !logs.contains(&signed.signature().r[2..]) && !logs.contains(&signed.signature().s[2..]),
        "a signature is in the logs"
    );
    assert!(longest_hex_run(&logs) < 64, "a 64-hex-digit secret-like run is in the logs:\n{logs}");
}
