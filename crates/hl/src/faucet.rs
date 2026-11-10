//! Hyperliquid testnet's faucet ("drip"): mock USDC for testing.
//!
//! The testnet app's Faucet page (`/drip`) signs nothing. It posts
//! `{"type": "claimDrip", "user": "0x…"}` to the testnet `/info` endpoint and reads the
//! answer: `null` means the drip went through, a string is a refusal in the faucet's words,
//! and HTTP 429 means rate limited. Hyperliquid's docs (Onboarding › Testnet faucet) say an
//! address can claim only after it has deposited on mainnet with the same address, and then
//! receives 1,000 mock USDC.
//!
//! [`crate::client::Client::claim_drip`] sends the request. Like every request in this crate,
//! it can only go to the testnet API ([`crate::network::API_URL`]).

use alloy_primitives::Address;
use serde_json::{Value, json};

/// Mock USDC per drip, as Hyperliquid documents it. The faucet's answer doesn't say.
pub const DRIP_USDC: f64 = 1000.0;

/// What the faucet said.
#[derive(Debug, Clone, PartialEq)]
pub enum DripOutcome {
    /// The faucet sent the drip: `usdc` mock USDC.
    Claimed { usdc: f64 },
    /// The address has never deposited on Hyperliquid mainnet, which the faucet requires.
    NoMainnetDeposit,
    /// The faucet has already paid this address.
    AlreadyClaimed,
    /// Too many requests (HTTP 429).
    RateLimited,
    /// Any other refusal, in the faucet's words (addresses masked, at most 200 characters).
    Refused(String),
    /// No usable answer: no connection, a timeout, HTTP 5xx or an answer that isn't JSON.
    Network(String),
}

impl DripOutcome {
    /// A short name for logs (never the address or the faucet's text).
    pub fn label(&self) -> &'static str {
        match self {
            DripOutcome::Claimed { .. } => "claimed",
            DripOutcome::NoMainnetDeposit => "refused: no mainnet deposit",
            DripOutcome::AlreadyClaimed => "refused: already claimed",
            DripOutcome::RateLimited => "rate limited",
            DripOutcome::Refused(_) => "refused",
            DripOutcome::Network(_) => "no answer",
        }
    }
}

/// The `/info` body that asks for a drip for `user`.
pub fn request_body(user: &Address) -> Value {
    json!({"type": "claimDrip", "user": crate::key::address_hex(user)})
}

/// Read the faucet's answer: its HTTP status and body text.
pub fn classify(status: u16, body: &str) -> DripOutcome {
    if status == 429 {
        return DripOutcome::RateLimited;
    }
    if status >= 500 {
        return DripOutcome::Network(format!("HTTP {status}"));
    }
    let parsed = serde_json::from_str::<Value>(body);
    if !(200..300).contains(&status) {
        // Any other 4xx is a refusal, in whatever words came with it.
        return match parsed {
            Ok(Value::String(s)) => refusal(&s),
            _ if body.trim().is_empty() => DripOutcome::Refused(format!("HTTP {status}")),
            _ => refusal(body.trim()),
        };
    }
    match parsed {
        Ok(Value::Null) => DripOutcome::Claimed { usdc: DRIP_USDC },
        Ok(Value::String(s)) => refusal(&s),
        // The testnet app doesn't know this shape either. Show it as it came.
        Ok(other) => DripOutcome::Refused(short(&other.to_string())),
        Err(_) => DripOutcome::Network("the answer wasn't JSON".into()),
    }
}

/// A refusal in the faucet's words, sorted by the reasons it gives.
fn refusal(text: &str) -> DripOutcome {
    let lower = text.to_ascii_lowercase();
    if lower.contains("does not exist on mainnet") {
        // Seen live: "Cannot claim drip because user 0x… does not exist on mainnet."
        DripOutcome::NoMainnetDeposit
    } else if lower.contains("already") {
        DripOutcome::AlreadyClaimed
    } else if lower.contains("rate limit") || lower.contains("too many") {
        DripOutcome::RateLimited
    } else {
        DripOutcome::Refused(short(text))
    }
}

/// Addresses masked, at most 200 characters.
fn short(text: &str) -> String {
    let masked = crate::key::mask_addresses(text.trim());
    if masked.chars().count() <= 200 {
        return masked;
    }
    let mut cut: String = masked.chars().take(199).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::client::{Client, REQUEST_TIMEOUT};

    const USER: &str = "0x00000000000000000000000000000000000000aa";

    fn user() -> Address {
        crate::key::parse_address(USER).unwrap()
    }

    /// The MOCK fixtures in `tests/fixtures/faucet_mock.json`.
    fn cases() -> Vec<(String, u16, String, String)> {
        let text = include_str!("../tests/fixtures/faucet_mock.json");
        let v: Value = serde_json::from_str(text).unwrap();
        assert!(v["about"].as_str().unwrap().starts_with("MOCK"), "the fixtures say they are mocks");
        v["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_string(),
                    c["status"].as_u64().unwrap() as u16,
                    c["body"].as_str().unwrap().to_string(),
                    c["expect"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn matches(outcome: &DripOutcome, expect: &str) -> bool {
        match (outcome, expect) {
            (DripOutcome::Claimed { usdc }, "claimed") => *usdc == DRIP_USDC,
            (DripOutcome::NoMainnetDeposit, "no_mainnet_deposit") => true,
            (DripOutcome::AlreadyClaimed, "already_claimed") => true,
            (DripOutcome::RateLimited, "rate_limited") => true,
            (DripOutcome::Network(_), "network") => true,
            (DripOutcome::Refused(text), e) => e.strip_prefix("refused:") == Some(text.as_str()),
            _ => false,
        }
    }

    #[test]
    fn the_body_asks_for_a_drip_for_the_user() {
        assert_eq!(request_body(&user()), json!({"type": "claimDrip", "user": USER}));
        // Checksummed input still goes out as the lower-case address.
        let mixed = crate::key::parse_address("0x5e9EE1089755c3435139848E47E6635505D5A13A").unwrap();
        assert_eq!(request_body(&mixed)["user"], "0x5e9ee1089755c3435139848e47e6635505d5a13a");
    }

    /// The request the client sends: a POST of that body to the testnet `/info` URL, with
    /// the same deadline as every other request.
    #[test]
    fn the_request_goes_to_the_testnet_info_url() {
        let req = Client::new().unwrap().claim_drip_request(&user()).unwrap();
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().as_str(), "https://api.hyperliquid-testnet.xyz/info");
        assert_eq!(req.timeout(), Some(&REQUEST_TIMEOUT));
        assert_eq!(req.headers()["content-type"], "application/json");
        let body: Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body, json!({"type": "claimDrip", "user": USER}));
    }

    #[test]
    fn every_mock_answer_is_read_as_its_outcome() {
        let cases = cases();
        assert!(cases.len() >= 8);
        for (name, status, body, expect) in cases {
            let got = classify(status, &body);
            assert!(matches(&got, &expect), "{name}: got {got:?}, want {expect}");
        }
    }

    #[test]
    fn refusals_hide_the_address_and_stay_short() {
        let long = format!("\"Drip refused for 0x5e9ee1089755c3435139848e47e6635505d5a13a: {}\"", "x".repeat(400));
        let DripOutcome::Refused(text) = classify(200, &long) else { panic!("not a refusal") };
        assert!(!text.contains("5e9ee1089755c3435139848e47e6635505d5a13a"), "{text}");
        assert!(text.contains("0x5e9e…a13a"), "{text}");
        assert!(text.chars().count() <= 200, "{}", text.len());
        // The known refusal carries no text at all.
        assert_eq!(
            classify(
                200,
                "\"Cannot claim drip because user 0x5e9ee1089755c3435139848e47e6635505d5a13a does not exist on mainnet.\""
            ),
            DripOutcome::NoMainnetDeposit
        );
        assert_eq!(classify(200, "\"Already claimed\""), DripOutcome::AlreadyClaimed);
    }

    #[test]
    fn labels_name_the_outcome_only() {
        let all = [
            DripOutcome::Claimed { usdc: DRIP_USDC },
            DripOutcome::NoMainnetDeposit,
            DripOutcome::AlreadyClaimed,
            DripOutcome::RateLimited,
            DripOutcome::Refused(format!("about {USER}")),
            DripOutcome::Network(format!("about {USER}")),
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(DripOutcome::label).collect();
        assert_eq!(labels.len(), all.len(), "each outcome has its own label");
        assert!(labels.iter().all(|l| !l.contains("0x")));
    }

    /// One request seen by [`mock_faucet`].
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        content_type: String,
        body: Value,
    }

    /// A local MOCK of the faucet: answers every request with `status` and `body`, and
    /// records what it was sent.
    async fn mock_faucet(status: u16, body: String) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, rest) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (String::from_utf8_lossy(&buf[..i]).to_string(), buf[i + 4..].to_vec());
                    }
                };
                let header = |name: &str| {
                    head.lines()
                        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)))
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default()
                };
                let len: usize = header("content-length").parse().unwrap_or(0);
                let mut req_body = rest;
                while req_body.len() < len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req_body.extend_from_slice(&chunk[..n]);
                }
                let mut first = head.lines().next().unwrap_or("").split(' ');
                log.lock().unwrap().push(Seen {
                    method: first.next().unwrap_or("").into(),
                    path: first.next().unwrap_or("").into(),
                    content_type: header("content-type"),
                    body: serde_json::from_slice(&req_body).unwrap_or(Value::Null),
                });
                let answer = format!(
                    "HTTP/1.1 {status} MOCK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(answer.as_bytes()).await;
            }
        });
        (base, seen)
    }

    /// Each MOCK answer, served over HTTP, comes back from `claim_drip` as its outcome, after
    /// exactly one POST of the claimDrip body to `/info`.
    #[tokio::test]
    async fn claim_drip_posts_once_and_reads_each_mock_answer() {
        for (name, status, body, expect) in cases() {
            let (base, seen) = mock_faucet(status, body).await;
            let got = Client::mock(&base).unwrap().claim_drip(&user()).await;
            assert!(matches(&got, &expect), "{name}: got {got:?}, want {expect}");
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{name}: {seen:?}");
            assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("POST", "/info"), "{name}");
            assert_eq!(seen[0].content_type, "application/json", "{name}");
            assert_eq!(seen[0].body, json!({"type": "claimDrip", "user": USER}), "{name}");
        }
    }

    /// No faucet at all, or one that never answers: a network error, after the deadline.
    #[tokio::test]
    async fn no_answer_is_a_network_error() {
        let closed = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://localhost:{}", l.local_addr().unwrap().port())
        };
        let got = Client::mock(&closed).unwrap().claim_drip(&user()).await;
        assert!(matches!(got, DripOutcome::Network(_)), "{got:?}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // read nothing, answer nothing
            }
        });
        let c = Client::mock(&base).unwrap().with_timeout(std::time::Duration::from_millis(300));
        let t0 = std::time::Instant::now();
        let got = c.claim_drip(&user()).await;
        assert!(matches!(&got, DripOutcome::Network(e) if e.contains("timed out")), "{got:?}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(3));
    }
}
