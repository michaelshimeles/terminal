//! **Test stand-in, not Turnkey.** A local server with the few Turnkey API routes Terminal
//! calls and a pretend sign-in page, for tests and for screenshots. Its JSON follows Turnkey's
//! API reference and `turnkey_client` 0.17's types (`whoami`, `list_wallets`,
//! `list_wallet_accounts`, `sign_raw_payload` V2); nothing here was recorded from Turnkey.
//!
//! It checks requests the way Turnkey does for these routes: the `X-Stamp` must be a valid
//! P-256 signature over the exact body, by a session key it issued and that hasn't expired,
//! for its organization. The wallet is a local secp256k1 key.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use k256::ecdsa::SigningKey as WalletKey;
use p256::ecdsa::signature::Verifier;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::callback::percent_decode;
use crate::session::{Claims, SESSION_SECONDS, unsigned_token};

pub const ORG_ID: &str = "00000000-mock-4000-8000-0000000000aa";
pub const USER_ID: &str = "00000000-mock-4000-8000-0000000000bb";
pub const WALLET_ID: &str = "00000000-mock-4000-8000-0000000000cc";
pub const USERNAME: &str = "test user (mock)";

struct State {
    wallet: WalletKey,
    /// Session public keys this mock issued, with their expiry (Unix seconds).
    sessions: Vec<(String, u64)>,
    /// Every API request that got past the stamp check: `(path, body)`.
    requests: Vec<(String, Value)>,
}

/// A running mock. Dropping it doesn't stop the server task; tests just let it end.
#[derive(Clone)]
pub struct MockTurnkey {
    pub base: String,
    state: Arc<Mutex<State>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl MockTurnkey {
    /// Start on 127.0.0.1 (port 0: any free port) with a fresh wallet key, or `wallet_hex`.
    pub async fn start(port: u16, wallet_hex: Option<&str>) -> Result<Self> {
        let wallet = match wallet_hex {
            Some(h) => WalletKey::from_slice(&hex::decode(h.trim_start_matches("0x"))?)?,
            None => WalletKey::random(&mut rand_core::OsRng),
        };
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let base = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let state = Arc::new(Mutex::new(State { wallet, sessions: Vec::new(), requests: Vec::new() }));
        let me = Self { base, state };
        let server = me.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let s = server.clone();
                tokio::spawn(async move {
                    let _ = s.handle(sock).await;
                });
            }
        });
        Ok(me)
    }

    /// The wallet's address (lower-case `0x…`).
    pub fn address(&self) -> String {
        let st = self.state.lock().unwrap();
        format!("{:#x}", alloy_primitives::Address::from_private_key(&st.wallet))
    }

    /// Issue a session for `public_key` (what the sign-in page's `stamp_login` does).
    pub fn mint_session(&self, public_key: &str, seconds: u64) -> String {
        let exp = now_secs() + seconds;
        self.state.lock().unwrap().sessions.push((public_key.to_ascii_lowercase(), exp));
        unsigned_token(&Claims {
            organization_id: ORG_ID.into(),
            user_id: USER_ID.into(),
            public_key: public_key.to_ascii_lowercase(),
            exp,
            session_type: "SESSION_TYPE_READ_WRITE".into(),
        })
    }

    /// The API requests it accepted, in order.
    pub fn requests(&self) -> Vec<(String, Value)> {
        self.state.lock().unwrap().requests.clone()
    }

    async fn handle(&self, mut sock: TcpStream) -> Result<()> {
        let (method, target, headers, body) = read(&mut sock).await?;
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let q = |k: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                .map(percent_decode)
                .unwrap_or_default()
        };
        match (method.as_str(), path) {
            ("GET", "/wallet/") | ("GET", "/wallet") => {
                write(&mut sock, "200 OK", "text/html; charset=utf-8", &[], &sign_in_page()).await
            }
            ("GET", "/wallet/complete") => {
                let (pk, state, port) = (q("pk"), q("state"), q("port"));
                let ok = pk.len() == 66 && pk.chars().all(|c| c.is_ascii_hexdigit()) && port.parse::<u16>().is_ok();
                if !ok {
                    return write(&mut sock, "400 Bad Request", "text/plain", &[], "bad request").await;
                }
                let token = self.mint_session(&pk, SESSION_SECONDS);
                let to = format!("http://127.0.0.1:{port}/callback?state={state}&session={token}");
                write(&mut sock, "302 Found", "text/plain", &[("location", &to)], "").await
            }
            ("POST", p) if p.starts_with("/public/v1/") => {
                let (status, reply) = match self.api(p, &headers, &body) {
                    Ok(v) => ("200 OK", v),
                    Err(e) => ("401 Unauthorized", json!({"code": 16, "message": format!("{e:#}")})),
                };
                write(&mut sock, status, "application/json", &[], &reply.to_string()).await
            }
            _ => write(&mut sock, "404 Not Found", "text/plain", &[], "not found").await,
        }
    }

    fn api(&self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Value> {
        let stamp = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-stamp"))
            .map(|(_, v)| v.clone())
            .context("missing X-Stamp")?;
        let stamp: Value = serde_json::from_slice(&BASE64_URL_SAFE_NO_PAD.decode(stamp.trim())?)?;
        anyhow::ensure!(stamp["scheme"] == "SIGNATURE_SCHEME_TK_API_P256", "unsupported stamp scheme");
        let pk = stamp["publicKey"].as_str().context("stamp has no publicKey")?.to_ascii_lowercase();
        let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&hex::decode(&pk)?)?;
        let sig = p256::ecdsa::Signature::from_der(&hex::decode(stamp["signature"].as_str().unwrap_or_default())?)?;
        key.verify(body, &sig).context("the stamp doesn't match the body")?;
        let v: Value = serde_json::from_slice(body)?;
        let mut st = self.state.lock().unwrap();
        anyhow::ensure!(
            st.sessions.iter().any(|(k, exp)| *k == pk && *exp > now_secs()),
            "could not find a valid session for this public key"
        );
        anyhow::ensure!(v["organizationId"] == ORG_ID, "organization mismatch");
        st.requests.push((path.to_string(), v.clone()));
        let address = format!("{:#x}", alloy_primitives::Address::from_private_key(&st.wallet));
        Ok(match path {
            "/public/v1/query/whoami" => {
                json!({"organizationId": ORG_ID, "organizationName": "mock sub-organization", "userId": USER_ID, "username": USERNAME})
            }
            "/public/v1/query/list_wallets" => json!({"wallets": [{"walletId": WALLET_ID, "walletName": "Terminal"}]}),
            "/public/v1/query/list_wallet_accounts" => json!({"accounts": [{
                "walletAccountId": "00000000-mock-4000-8000-0000000000dd",
                "organizationId": ORG_ID,
                "walletId": WALLET_ID,
                "curve": "CURVE_SECP256K1",
                "pathFormat": "PATH_FORMAT_BIP32",
                "path": "m/44'/60'/0'/0/0",
                "addressFormat": "ADDRESS_FORMAT_ETHEREUM",
                "address": alloy_primitives::Address::from_private_key(&st.wallet).to_string(),
            }]}),
            "/public/v1/submit/sign_raw_payload" => {
                let p = &v["parameters"];
                anyhow::ensure!(v["type"] == "ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2", "unexpected activity type");
                anyhow::ensure!(p["encoding"] == "PAYLOAD_ENCODING_HEXADECIMAL", "unexpected encoding");
                anyhow::ensure!(p["hashFunction"] == "HASH_FUNCTION_NO_OP", "unexpected hash function");
                anyhow::ensure!(
                    p["signWith"].as_str().unwrap_or_default().eq_ignore_ascii_case(&address),
                    "signWith is not this organization's wallet"
                );
                let digest = hex::decode(p["payload"].as_str().unwrap_or_default())?;
                anyhow::ensure!(digest.len() == 32, "the payload is not a 32-byte digest");
                let (sig, recid) = st.wallet.sign_prehash_recoverable(&digest)?;
                let (r, s) = sig.split_bytes();
                json!({"activity": {
                    "id": "00000000-mock-4000-8000-0000000000ee",
                    "organizationId": ORG_ID,
                    "status": "ACTIVITY_STATUS_COMPLETED",
                    "type": "ACTIVITY_TYPE_SIGN_RAW_PAYLOAD_V2",
                    "result": {"signRawPayloadResult": {
                        "r": hex::encode(r),
                        "s": hex::encode(s),
                        "v": format!("{:02x}", u8::from(recid.is_y_odd())),
                    }},
                }})
            }
            _ => anyhow::bail!("this mock doesn't serve {path}"),
        })
    }
}

type Request = (String, String, Vec<(String, String)>, Vec<u8>);

async fn read(sock: &mut TcpStream) -> Result<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "connection closed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        anyhow::ensure!(buf.len() < 64 * 1024, "headers too large");
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "connection closed");
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok((method, target, headers, body))
}

async fn write(sock: &mut TcpStream, status: &str, ctype: &str, extra: &[(&str, &str)], body: &str) -> Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n", body.len());
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("access-control-allow-origin: *\r\nconnection: close\r\n\r\n");
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}

/// The pretend sign-in page: one button that issues a session for the device's key. Like the
/// real page it reads its parameters from the URL fragment.
fn sign_in_page() -> String {
    let style = "body{font:15px -apple-system,system-ui,sans-serif;background:#0b0b0c;color:#e6e6e6;display:grid;place-items:center;height:100vh;margin:0}main{width:420px}.b{background:#f5a623;color:#000;border-radius:9px;padding:2px 8px;font:600 12px ui-monospace,monospace}.m{color:#9a9a9a;font-size:13px}a.go{display:block;margin-top:18px;padding:10px;background:#2a6df4;color:#fff;text-align:center;border-radius:8px;text-decoration:none}input{width:100%;padding:8px;box-sizing:border-box}";
    let script = "const p=new URLSearchParams(location.hash.slice(1));const ok=/^0[23][0-9a-f]{64}$/.test(p.get('pk')||'')&&/^[0-9a-f]{64}$/.test(p.get('state')||'')&&/^[0-9]{1,5}$/.test(p.get('port')||'');if(p.get('mode')==='export'){document.getElementById('f').innerHTML='<p>Export happens in Turnkey\\'s own iframe on the real page. This mock shows no recovery phrase.</p>'}else if(!ok){document.getElementById('f').textContent='Bad sign-in link.'}else{document.getElementById('go').href='/wallet/complete?pk='+p.get('pk')+'&state='+p.get('state')+'&port='+p.get('port');document.getElementById('k').textContent=p.get('pk').slice(0,10)}";
    format!(
        "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>Terminal wallet (mock)</title><style>{style}</style><main><h2>Terminal wallet <span class=b>TESTNET</span></h2><p class=m>MOCK TURNKEY (local test server, not Turnkey)</p><div id=f><p class=m>Sign in or create your wallet. The real page sends a code to your email (Turnkey).</p><p>Email<br><input value=\"test@example.com\"></p><a class=go id=go href=#>Continue</a><p class=m>The session is issued for this device's key <span id=k></span>…</p></div></main><script>{script}</script>"
    )
}
